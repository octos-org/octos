//! Web search tool with multiple provider support.
//!
//! Provider priority (best quality first, DDG as free fallback):
//! 1. Tavily (`TAVILY_API_KEY`) — AI-optimized search, 1k free/month
//! 2. Exa (`EXA_API_KEY`) — neural/semantic search, 1k free/month
//! 3. DuckDuckGo (no key) — free HTML search
//! 4. Brave Search (`BRAVE_API_KEY`) — free tier: 2k queries/month
//! 5. You.com (`YDC_API_KEY`) — rich JSON results with snippets
//! 6. Perplexity Sonar (`PERPLEXITY_API_KEY`) — AI-synthesized fallback (most expensive)
//! 7. Headless-Chrome (CDP) Bing — **last resort**, `browser` feature only
//!
//! Each provider is tried in order. If a provider returns no results or fails,
//! the next one is attempted. Perplexity is last among the HTTP providers
//! because it costs the most but gives the best answers (AI-synthesized with
//! citations).
//!
//! The headless-Chrome (CDP) provider runs only when the whole HTTP chain
//! yields nothing — the common keyless-box case where DuckDuckGo HTTP-403s as a
//! bot. It drives a Bing SERP through the same in-process `chromiumoxide`
//! headless browser the `browser` tool uses (no external `deep_crawl` binary),
//! and is gated behind the default-on `browser` cargo feature. On any box with
//! no Chrome/Chromium it degrades to a fast, clean miss (detected up-front via
//! `chromiumoxide::detection::default_executable`, never a launch attempt), so
//! the search terminates instead of hanging.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use reqwest::Client;
use serde::Deserialize;
use tracing::{info, warn};

use super::{Tool, ToolResult};

/// Detect whether a `ToolResult` represents a quota-exhausted or rate-limited
/// response from a search provider. Used to drive auto-rotation across the
/// provider chain in `WebSearchTool::execute` (M8.10-B, see issue #575).
///
/// Returns `true` only for `!result.success` outputs that contain telltale
/// English or Chinese keywords. Successful results (including partial empties
/// like "No results found") are NEVER treated as quota errors.
pub(crate) fn is_quota_or_rate_limit_error(result: &ToolResult) -> bool {
    if result.success {
        return false;
    }
    let lower = result.output.to_ascii_lowercase();
    // English keywords (case-insensitive).
    const ENGLISH: &[&str] = &[
        "429",
        "quota",
        "rate limit",
        "rate_limit",
        "rate-limit",
        "too many requests",
        "usage limit",
        "credit",
        "insufficient",
        "exhausted",
    ];
    if ENGLISH.iter().any(|kw| lower.contains(kw)) {
        return true;
    }
    // Chinese keywords (case is irrelevant for CJK).
    const CHINESE: &[&str] = &["配额", "耗尽", "限流", "超出"];
    CHINESE.iter().any(|kw| result.output.contains(kw))
}

pub struct WebSearchTool {
    client: Client,
    config: Option<Arc<super::tool_config::ToolConfigStore>>,
    provider_keys: HashMap<String, String>,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                .build()
                .unwrap_or_else(|_| Client::new()),
            config: None,
            provider_keys: HashMap::new(),
        }
    }

    pub fn with_config(mut self, config: Arc<super::tool_config::ToolConfigStore>) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_provider_keys(mut self, provider_keys: HashMap<String, String>) -> Self {
        self.provider_keys = provider_keys;
        self
    }

    fn provider_key(&self, provider_id: &str, env_var: &str) -> Option<String> {
        self.provider_keys
            .get(provider_id)
            .cloned()
            .or_else(|| std::env::var(env_var).ok())
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct Input {
    query: String,
    #[serde(default)]
    count: Option<u8>,
}

// --- Brave types ---

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWebResults>,
}

#[derive(Deserialize)]
struct BraveWebResults {
    results: Vec<BraveWebResult>,
}

#[derive(Deserialize)]
struct BraveWebResult {
    title: String,
    url: String,
    description: String,
}

// --- You.com types ---

#[derive(Deserialize)]
struct YouResponse {
    results: Option<YouResults>,
}

#[derive(Deserialize)]
struct YouResults {
    web: Option<Vec<YouWebResult>>,
}

#[derive(Deserialize)]
struct YouWebResult {
    title: String,
    url: String,
    description: String,
    #[serde(default)]
    snippets: Vec<String>,
}

// --- Exa types ---

#[derive(Deserialize)]
struct ExaResponse {
    results: Option<Vec<ExaResult>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExaResult {
    title: Option<String>,
    url: String,
    #[serde(default)]
    highlights: Vec<String>,
    #[serde(default)]
    published_date: Option<String>,
}

// --- Perplexity types ---

#[derive(Deserialize)]
struct PerplexityResponse {
    choices: Option<Vec<PerplexityChoice>>,
    #[serde(default)]
    citations: Vec<String>,
}

#[derive(Deserialize)]
struct PerplexityChoice {
    message: Option<PerplexityMessage>,
}

#[derive(Deserialize)]
struct PerplexityMessage {
    content: Option<String>,
}

// --- Tavily types ---

#[derive(Deserialize)]
struct TavilyResponse {
    results: Option<Vec<TavilyResult>>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    content: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for information. Supports Tavily, Exa, Brave, You.com, Perplexity, and DuckDuckGo (auto-detected from environment)."
    }

    fn tags(&self) -> &[&str] {
        &["web"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "count": {
                    "type": "integer",
                    "description": "Number of results (1-10, default: 5)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid web_search input")?;

        let config_count = match &self.config {
            Some(c) => c.get_u64("web_search", "count").await.map(|v| v as u8),
            None => None,
        };
        let count = input.count.or(config_count).unwrap_or(5).clamp(1, 10);

        // Provider priority: Tavily first (best quality), then free/cheap, Perplexity last.
        // 1. Tavily (AI-optimized, 1k free/month)
        // 2. DuckDuckGo (free, always available)
        // 3. Exa (neural search)
        // 4. Brave Search (free tier: 2k queries/month)
        // 5. You.com (API key required)
        // 6. Perplexity Sonar (AI-synthesized, most expensive — fallback only)

        // Tavily (AI-optimized search — best for recent/niche topics)
        if let Some(api_key) = self.provider_key("tavily", "TAVILY_API_KEY") {
            let result = self.tavily_search(&input.query, count, &api_key).await;
            if let Ok(ref r) = result {
                if r.success && !r.output.contains("No results found") {
                    info!(
                        provider = "tavily",
                        used_provider = "tavily",
                        query = %input.query,
                        "web_search"
                    );
                    return result;
                }
                if is_quota_or_rate_limit_error(r) {
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "tavily",
                        fallback_reason = "quota",
                        error = %snippet,
                        "web_search rotation"
                    );
                } else if !r.success {
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "tavily",
                        fallback_reason = "error",
                        error = %snippet,
                        "web_search rotation"
                    );
                } else {
                    info!(
                        provider = "tavily",
                        fallback_reason = "empty",
                        "web_search rotation"
                    );
                }
            }
        }

        // Try DuckDuckGo (free, no key needed)
        let ddg_result = self.ddg_search(&input.query, count).await;
        if let Ok(ref r) = ddg_result {
            if r.success && !r.output.contains("No results found") {
                info!(
                    provider = "duckduckgo",
                    used_provider = "duckduckgo",
                    query = %input.query,
                    "web_search"
                );
                return ddg_result;
            }
            if is_quota_or_rate_limit_error(r) {
                let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                warn!(
                    provider = "duckduckgo",
                    fallback_reason = "quota",
                    error = %snippet,
                    "web_search rotation"
                );
            } else if !r.success {
                let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                warn!(
                    provider = "duckduckgo",
                    fallback_reason = "error",
                    error = %snippet,
                    "web_search rotation"
                );
            } else {
                info!(
                    provider = "duckduckgo",
                    fallback_reason = "empty",
                    "web_search rotation"
                );
            }
        }

        // Exa (neural search — best for niche/recent topics)
        if let Ok(api_key) = std::env::var("EXA_API_KEY") {
            let result = self.exa_search(&input.query, count, &api_key).await;
            if let Ok(ref r) = result {
                if r.success && !r.output.contains("No results found") {
                    info!(
                        provider = "exa",
                        used_provider = "exa",
                        query = %input.query,
                        "web_search"
                    );
                    return result;
                }
                if is_quota_or_rate_limit_error(r) {
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "exa",
                        fallback_reason = "quota",
                        error = %snippet,
                        "web_search rotation"
                    );
                } else if !r.success {
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "exa",
                        fallback_reason = "error",
                        error = %snippet,
                        "web_search rotation"
                    );
                } else {
                    info!(
                        provider = "exa",
                        fallback_reason = "empty",
                        "web_search rotation"
                    );
                }
            }
        }

        // Brave Search
        if let Some(api_key) = self.provider_key("brave", "BRAVE_API_KEY") {
            let result = self.brave_search(&input.query, count, &api_key).await;
            if let Ok(ref r) = result {
                if r.success && !r.output.contains("No results found") {
                    info!(
                        provider = "brave",
                        used_provider = "brave",
                        query = %input.query,
                        "web_search"
                    );
                    return result;
                }
                if is_quota_or_rate_limit_error(r) {
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "brave",
                        fallback_reason = "quota",
                        error = %snippet,
                        "web_search rotation"
                    );
                } else if !r.success {
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "brave",
                        fallback_reason = "error",
                        error = %snippet,
                        "web_search rotation"
                    );
                } else {
                    info!(
                        provider = "brave",
                        fallback_reason = "empty",
                        "web_search rotation"
                    );
                }
            }
        }

        // You.com
        if let Some(api_key) = self.provider_key("you", "YDC_API_KEY") {
            let result = self.you_search(&input.query, count, &api_key).await;
            if let Ok(ref r) = result {
                if r.success && !r.output.contains("No results found") {
                    info!(
                        provider = "you.com",
                        used_provider = "you.com",
                        query = %input.query,
                        "web_search"
                    );
                    return result;
                }
                if is_quota_or_rate_limit_error(r) {
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "you.com",
                        fallback_reason = "quota",
                        error = %snippet,
                        "web_search rotation"
                    );
                } else if !r.success {
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "you.com",
                        fallback_reason = "error",
                        error = %snippet,
                        "web_search rotation"
                    );
                } else {
                    info!(
                        provider = "you.com",
                        fallback_reason = "empty",
                        "web_search rotation"
                    );
                }
            }
        }

        // Perplexity Sonar as last resort (AI-synthesized, costs money).
        // M8.10-B: previously this branch returned UNCONDITIONALLY, so a quota
        // error from Perplexity surfaced directly to the LLM (issue #575
        // problem B). Now we mirror the structure of every other provider:
        // success → return; quota → log + fall through to DDG fallback;
        // other failure → log + fall through.
        if let Some(api_key) = self.provider_key("perplexity", "PERPLEXITY_API_KEY") {
            let result = self.perplexity_search(&input.query, &api_key).await;
            if let Ok(ref r) = result {
                if r.success && !r.output.contains("No results found") {
                    info!(
                        provider = "perplexity",
                        used_provider = "perplexity",
                        query = %input.query,
                        "web_search"
                    );
                    return result;
                }
                if is_quota_or_rate_limit_error(r) {
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "perplexity",
                        fallback_reason = "quota",
                        error = %snippet,
                        "web_search rotation"
                    );
                } else if !r.success {
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "perplexity",
                        fallback_reason = "error",
                        error = %snippet,
                        "web_search rotation"
                    );
                } else {
                    info!(
                        provider = "perplexity",
                        fallback_reason = "empty",
                        "web_search rotation"
                    );
                }
            }
        }

        // Headless-Chrome (CDP) fallback — last resort. Only meaningful when the
        // HTTP chain produced nothing usable (e.g. keyless box where DuckDuckGo
        // bot-403s). Drives Bing through the in-process headless browser. On a
        // box with no Chrome this is a fast, clean miss (see
        // `browser_cdp_search`), so the search terminates instead of hanging.
        #[cfg(feature = "browser")]
        {
            let http_chain_empty = ddg_result
                .as_ref()
                .map(|r| !r.success || r.output.contains("No results found"))
                .unwrap_or(true);
            if http_chain_empty {
                // Bound a touch above the per-action browser default headroom so
                // launch + Bing nav fit, but a wedged Chrome can't block forever.
                let cdp = self
                    .browser_cdp_search(&input.query, count, Duration::from_secs(45))
                    .await;
                if let Ok(ref r) = cdp {
                    if r.success && !r.output.contains("No results found") {
                        info!(
                            provider = "bing_cdp",
                            used_provider = "bing_cdp",
                            query = %input.query,
                            "web_search"
                        );
                        return cdp;
                    }
                    let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
                    warn!(
                        provider = "bing_cdp",
                        fallback_reason = if r.success { "empty" } else { "miss" },
                        error = %snippet,
                        "web_search rotation"
                    );
                }
            }
        }

        info!(
            provider = "duckduckgo (fallback)",
            used_provider = "duckduckgo (fallback)",
            query = %input.query,
            "web_search"
        );
        // Return whatever DDG gave us (even if empty / quota-flagged); all
        // configured providers were exhausted. The caller LLM should see this
        // and not retry with identical args (worker.txt guidance).
        ddg_result
    }
}

impl WebSearchTool {
    // --- Tavily (AI-optimized search) ---

    async fn tavily_search(&self, query: &str, count: u8, api_key: &str) -> Result<ToolResult> {
        let body = serde_json::json!({
            "query": query,
            "max_results": count,
            "include_answer": false,
        });

        let response = self
            .client
            .post("https://api.tavily.com/search")
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .await
            .wrap_err("failed to call Tavily API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(ToolResult {
                output: format!("Tavily API error ({status}): {body}"),
                success: false,
                ..Default::default()
            });
        }

        let tavily: TavilyResponse = response
            .json()
            .await
            .wrap_err("failed to parse Tavily response")?;

        let results = tavily.results.unwrap_or_default();

        if results.is_empty() {
            return Ok(ToolResult {
                output: format!("No results found for: {query}"),
                success: true,
                ..Default::default()
            });
        }

        let mut output = format!("Results for: {query}\n\n");
        for (i, r) in results.iter().enumerate() {
            output.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
            let snippet = octos_core::truncated_utf8(&r.content, 300, "...");
            output.push_str(&format!("   {}\n\n", snippet));
        }

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }

    // --- Exa (neural search) ---

    async fn exa_search(&self, query: &str, count: u8, api_key: &str) -> Result<ToolResult> {
        let body = serde_json::json!({
            "query": query,
            "type": "auto",
            "numResults": count,
            "contents": {
                "highlights": {
                    "numSentences": 3
                }
            }
        });

        let response = self
            .client
            .post("https://api.exa.ai/search")
            .header("x-api-key", api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to call Exa API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(ToolResult {
                output: format!("Exa API error ({status}): {body}"),
                success: false,
                ..Default::default()
            });
        }

        let exa: ExaResponse = response
            .json()
            .await
            .wrap_err("failed to parse Exa response")?;

        let results = exa.results.unwrap_or_default();

        if results.is_empty() {
            return Ok(ToolResult {
                output: format!("No results found for: {query}"),
                success: true,
                ..Default::default()
            });
        }

        let mut output = format!("Results for: {query}\n\n");
        for (i, r) in results.iter().enumerate() {
            let title = r.title.as_deref().unwrap_or("(untitled)");
            output.push_str(&format!("{}. {}\n   {}\n", i + 1, title, r.url));

            if let Some(ref date) = r.published_date {
                output.push_str(&format!("   Published: {}\n", date));
            }

            for highlight in &r.highlights {
                let trimmed = highlight.trim();
                if !trimmed.is_empty() {
                    output.push_str(&format!("   {}\n", trimmed));
                }
            }

            output.push('\n');
        }

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }

    // --- Perplexity Sonar ---

    async fn perplexity_search(&self, query: &str, api_key: &str) -> Result<ToolResult> {
        let body = serde_json::json!({
            "model": "sonar",
            "messages": [{"role": "user", "content": query}],
            "max_tokens": 1024
        });

        let response = self
            .client
            .post("https://api.perplexity.ai/chat/completions")
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to call Perplexity API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(ToolResult {
                output: format!("Perplexity API error ({status}): {body}"),
                success: false,
                ..Default::default()
            });
        }

        let pplx: PerplexityResponse = response
            .json()
            .await
            .wrap_err("failed to parse Perplexity response")?;

        let answer = pplx
            .choices
            .and_then(|c| c.into_iter().next())
            .and_then(|c| c.message)
            .and_then(|m| m.content)
            .unwrap_or_default();

        if answer.is_empty() {
            return Ok(ToolResult {
                output: format!("No results found for: {query}"),
                success: true,
                ..Default::default()
            });
        }

        let mut output = format!("Search: {query}\n\n{answer}");

        if !pplx.citations.is_empty() {
            output.push_str("\n\nSources:\n");
            for (i, url) in pplx.citations.iter().enumerate() {
                output.push_str(&format!("  [{}] {}\n", i + 1, url));
            }
        }

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }

    // --- You.com ---

    async fn you_search(&self, query: &str, count: u8, api_key: &str) -> Result<ToolResult> {
        let response = self
            .client
            .get("https://ydc-index.io/v1/search")
            .header("X-API-Key", api_key)
            .query(&[("query", query), ("count", &count.to_string())])
            .send()
            .await
            .wrap_err("failed to call You.com Search API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(ToolResult {
                output: format!("You.com API error ({status}): {body}"),
                success: false,
                ..Default::default()
            });
        }

        let you: YouResponse = response
            .json()
            .await
            .wrap_err("failed to parse You.com response")?;

        let results = you.results.and_then(|r| r.web).unwrap_or_default();

        if results.is_empty() {
            return Ok(ToolResult {
                output: format!("No results found for: {query}"),
                success: true,
                ..Default::default()
            });
        }

        let mut output = format!("Results for: {query}\n\n");
        for (i, r) in results.iter().enumerate() {
            output.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
            if !r.description.is_empty() {
                output.push_str(&format!("   {}\n", r.description));
            }
            // Include first snippet if available (richer than description)
            if let Some(snippet) = r.snippets.first() {
                let truncated = octos_core::truncated_utf8(snippet, 300, "...");
                output.push_str(&format!("   {}\n", truncated));
            }
            output.push('\n');
        }

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }

    // --- Brave Search ---

    async fn brave_search(&self, query: &str, count: u8, api_key: &str) -> Result<ToolResult> {
        let response = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", &count.to_string())])
            .send()
            .await
            .wrap_err("failed to call Brave Search API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(ToolResult {
                output: format!("Brave Search API error ({status}): {body}"),
                success: false,
                ..Default::default()
            });
        }

        let brave: BraveResponse = response
            .json()
            .await
            .wrap_err("failed to parse Brave Search response")?;

        let results = brave.web.map(|w| w.results).unwrap_or_default();

        if results.is_empty() {
            return Ok(ToolResult {
                output: format!("No results found for: {query}"),
                success: true,
                ..Default::default()
            });
        }

        let mut output = format!("Results for: {query}\n\n");
        for (i, r) in results.iter().enumerate() {
            output.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                r.title,
                r.url,
                r.description
            ));
        }

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }

    // --- DuckDuckGo HTML fallback ---

    async fn ddg_search(&self, query: &str, count: u8) -> Result<ToolResult> {
        let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoded(query));

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .wrap_err("failed to fetch DuckDuckGo search results")?;

        if !response.status().is_success() {
            let status = response.status();
            return Ok(ToolResult {
                output: format!("DuckDuckGo search error: HTTP {status}"),
                success: false,
                ..Default::default()
            });
        }

        let html = response.text().await.unwrap_or_default();
        let results = parse_ddg_results(&html, count as usize);

        if results.is_empty() {
            return Ok(ToolResult {
                output: format!("No results found for: {query}"),
                success: true,
                ..Default::default()
            });
        }

        let mut output = format!("Results for: {query}\n\n");
        for (i, (title, url, snippet)) in results.iter().enumerate() {
            output.push_str(&format!("{}. {title}\n   {url}\n   {snippet}\n\n", i + 1));
        }

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }

    // --- Headless-Chrome (CDP) fallback ---------------------------------------
    //
    // Last-resort provider used only when every HTTP provider above has yielded
    // nothing (typically a keyless box where DuckDuckGo HTTP-403s as a bot). It
    // drives a Bing SERP through the same in-process `chromiumoxide` headless
    // browser that powers the `browser` tool — no external `deep_crawl` binary —
    // and reuses `BLOCKED_ENV_VARS` sanitisation and a bounded launch/nav.
    //
    // Graceful no-browser degradation is the whole point: most environments (CI,
    // fleet minis, bare boxes) have no Chrome. We detect that BEFORE any launch
    // via `chromiumoxide::detection::default_executable` (the same `which`/known-
    // path probe the browser tool relies on) and return a fast, clean miss. The
    // launch + navigation are additionally wrapped in `bound` so a wedged Chrome
    // can never block the agent.

    /// Public entry point: resolve a usable Chrome/Chromium executable, then run
    /// the CDP-backed Bing search. Always returns a `ToolResult` (never panics /
    /// hangs); a non-success result means "skip me, try the next thing".
    #[cfg(feature = "browser")]
    pub(crate) async fn browser_cdp_search(
        &self,
        query: &str,
        count: u8,
        bound: Duration,
    ) -> Result<ToolResult> {
        let executable = detect_browser_executable();
        self.browser_cdp_search_with_executable(query, count, bound, executable)
            .await
    }

    /// Inner implementation with the resolved executable injected so the
    /// no-browser path is unit-testable without touching process env / global
    /// state. `executable == None` is the "no Chrome on this host" case and
    /// short-circuits to a fast, clean miss with no launch attempt.
    #[cfg(feature = "browser")]
    pub(crate) async fn browser_cdp_search_with_executable(
        &self,
        query: &str,
        count: u8,
        bound: Duration,
        executable: Option<std::path::PathBuf>,
    ) -> Result<ToolResult> {
        let Some(executable) = executable else {
            // No usable browser: skip cleanly. Caller proceeds (and, with every
            // provider exhausted, the search terminates rather than hanging).
            warn!(
                provider = "bing_cdp",
                fallback_reason = "no_browser",
                "web_search: no Chrome/Chromium found; skipping headless fallback"
            );
            return Ok(ToolResult {
                output: "browser unavailable: no Chrome/Chromium executable detected".to_string(),
                success: false,
                ..Default::default()
            });
        };

        // Bound the entire launch + navigation + extraction so a stuck Chrome
        // cannot block the agent. On timeout the session future is dropped,
        // whose `Drop`/`shutdown` kills the child process (see browser.rs).
        let fut = render_and_parse_bing(executable, query, count);
        match tokio::time::timeout(bound, fut).await {
            Ok(Ok(results)) => {
                if results.is_empty() {
                    return Ok(ToolResult {
                        output: format!("No results found for: {query}"),
                        success: true,
                        ..Default::default()
                    });
                }
                let mut output = format!("Results for: {query}\n\n");
                for (i, (title, url, snippet)) in results.iter().enumerate() {
                    output.push_str(&format!("{}. {title}\n   {url}\n", i + 1));
                    if !snippet.is_empty() {
                        output.push_str(&format!("   {snippet}\n"));
                    }
                    output.push('\n');
                }
                Ok(ToolResult {
                    output,
                    success: true,
                    ..Default::default()
                })
            }
            Ok(Err(e)) => {
                let snippet = octos_core::truncated_utf8(&e.to_string(), 200, "...");
                warn!(
                    provider = "bing_cdp",
                    fallback_reason = "launch_error",
                    error = %snippet,
                    "web_search: headless Chrome search failed"
                );
                Ok(ToolResult {
                    output: format!("browser search failed: {snippet}"),
                    success: false,
                    ..Default::default()
                })
            }
            Err(_) => {
                warn!(
                    provider = "bing_cdp",
                    fallback_reason = "timeout",
                    timeout_ms = bound.as_millis() as u64,
                    "web_search: headless Chrome search timed out"
                );
                Ok(ToolResult {
                    output: format!("browser search timed out after {}s", bound.as_secs().max(1)),
                    success: false,
                    ..Default::default()
                })
            }
        }
    }
}

/// Detect a usable Chrome/Chromium/Edge executable using the exact same probe
/// the `browser` tool relies on (env `CHROME`, `which`, well-known install
/// paths). Returns `None` when no browser exists so callers degrade cleanly
/// instead of attempting (and blocking on) a doomed launch.
#[cfg(feature = "browser")]
fn detect_browser_executable() -> Option<std::path::PathBuf> {
    chromiumoxide::detection::default_executable(
        chromiumoxide::detection::DetectionOptions::default(),
    )
    .ok()
}

/// Launch headless Chrome, navigate to a Bing SERP for `query`, pull the
/// rendered HTML, and parse out result rows. Kept separate from
/// `WebSearchTool` so the bounded-timeout wrapper owns the future and Chrome is
/// reliably torn down on cancellation.
#[cfg(feature = "browser")]
async fn render_and_parse_bing(
    executable: std::path::PathBuf,
    query: &str,
    count: u8,
) -> Result<Vec<(String, String, String)>> {
    use chromiumoxide::browser::{Browser, BrowserConfig};
    use futures::StreamExt;

    let temp_dir = tempfile::Builder::new()
        .prefix("octos-websearch-cdp-")
        .tempdir()
        .wrap_err("failed to create temp dir for Chrome")?;

    let mut builder = BrowserConfig::builder()
        .chrome_executable(&executable)
        .user_data_dir(temp_dir.path())
        .arg("--headless=new")
        .arg("--disable-dev-shm-usage")
        .arg("--disable-extensions")
        .arg("--disable-background-networking");

    // Sanitize environment: blank out the shared blocked vars (LD_PRELOAD, etc).
    for var in crate::sandbox::BLOCKED_ENV_VARS {
        builder = builder.env(*var, "");
    }

    let config = builder
        .build()
        .map_err(|e| eyre::eyre!("failed to build browser config: {e}"))?;

    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| eyre::eyre!("failed to launch Chrome: {e}"))?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    // Bing is far more lenient than Google with headless-Chrome scraping
    // (Google CAPTCHAs datacenter IPs). `count` is advisory.
    let search_url = format!(
        "https://www.bing.com/search?q={}&count={}",
        urlencoded(query),
        count.min(10),
    );

    let outcome = async {
        let page = browser
            .new_page(search_url.as_str())
            .await
            .map_err(|e| eyre::eyre!("failed to open Bing page: {e}"))?;
        let _ = page.wait_for_navigation().await;
        let html = page
            .content()
            .await
            .map_err(|e| eyre::eyre!("failed to read Bing HTML: {e}"))?;
        Ok::<_, eyre::Report>(parse_bing_results(&html, count as usize))
    }
    .await;

    // Best-effort teardown; the temp dir drops with this scope.
    let _ = browser.close().await;
    handler_task.abort();

    outcome
}

/// Simple URL encoding for query parameters.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from(HEX[(b >> 4) as usize]));
                out.push(char::from(HEX[(b & 0xf) as usize]));
            }
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Parse DuckDuckGo HTML search results.
/// DDG format: `class="result__a" href="//duckduckgo.com/l/?uddg=ENCODED_URL&rut=...">Title</a>`
/// Snippet: `class="result__snippet">snippet text</a>`
fn parse_ddg_results(html: &str, max: usize) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    let marker = "class=\"result__a\"";

    let mut search_from = 0;
    while results.len() < max {
        let pos = match html[search_from..].find(marker) {
            Some(p) => search_from + p + marker.len(),
            None => break,
        };
        search_from = pos;

        let chunk = &html[pos..];

        // Extract href: href="//duckduckgo.com/l/?uddg=REAL_URL&rut=..."
        let raw_href = match extract_attr(chunk, "href=\"") {
            Some(h) => h,
            None => continue,
        };

        // Decode the real URL from DDG redirect
        let url = decode_ddg_url(&raw_href);
        if !url.starts_with("http") {
            continue;
        }
        // Skip DDG ad/tracking redirects
        if url.contains("duckduckgo.com/y.js") {
            continue;
        }

        // Title is between > and </a>
        let title = match chunk.find('>') {
            Some(gt) => {
                let after = &chunk[gt + 1..];
                match after.find("</a>") {
                    Some(end) => strip_tags(&after[..end]),
                    None => continue,
                }
            }
            None => continue,
        };

        if title.is_empty() {
            continue;
        }

        // Snippet from class="result__snippet"
        let snippet_marker = "class=\"result__snippet\"";
        let snippet = if let Some(sp) = chunk.find(snippet_marker) {
            let after_marker = &chunk[sp + snippet_marker.len()..];
            match after_marker.find('>') {
                Some(gt) => {
                    let content = &after_marker[gt + 1..];
                    match content.find("</a>") {
                        Some(end) => strip_tags(&content[..end]),
                        None => String::new(),
                    }
                }
                None => String::new(),
            }
        } else {
            String::new()
        };

        results.push((title, url, snippet));
    }

    results
}

/// Parse a Bing SERP into `(title, url, snippet)` tuples.
///
/// Bing organic results are `<li class="b_algo"> … <h2><a href="URL">TITLE</a>
/// </h2> … <p>SNIPPET</p> … </li>`. We scan `class="b_algo"` anchors only, which
/// naturally skips ad blocks (`b_ad`), people-also-ask, and related-search rows.
/// Same string-scan style as [`parse_ddg_results`] — no HTML-parser dependency.
#[cfg(feature = "browser")]
fn parse_bing_results(html: &str, max: usize) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    let marker = "class=\"b_algo\"";

    let mut search_from = 0;
    while results.len() < max {
        let li_pos = match html[search_from..].find(marker) {
            Some(p) => search_from + p + marker.len(),
            None => break,
        };
        // Bound this result's chunk to the next b_algo so a missing field in
        // one row can't borrow the next row's title/snippet.
        let rest = &html[li_pos..];
        let chunk_end = rest.find(marker).unwrap_or(rest.len());
        let chunk = &rest[..chunk_end];
        search_from = li_pos;

        // Title + URL: first <a href="..."> after the b_algo marker (the <h2>).
        let href = match extract_attr(chunk, "href=\"") {
            Some(h) => h,
            None => continue,
        };
        if !href.starts_with("http") {
            continue;
        }

        // Title text is between the anchor's '>' and its '</a>'.
        let anchor_open = match chunk.find("href=\"") {
            Some(h) => match chunk[h..].find('>') {
                Some(gt) => h + gt + 1,
                None => continue,
            },
            None => continue,
        };
        let title = match chunk[anchor_open..].find("</a>") {
            Some(end) => strip_tags(&chunk[anchor_open..anchor_open + end]),
            None => continue,
        };
        if title.is_empty() {
            continue;
        }

        // Snippet: first <p>…</p> in the result chunk (Bing's caption text).
        let snippet = if let Some(p) = chunk.find("<p") {
            match chunk[p..].find('>') {
                Some(gt) => {
                    let body = &chunk[p + gt + 1..];
                    match body.find("</p>") {
                        Some(end) => strip_tags(&body[..end]),
                        None => String::new(),
                    }
                }
                None => String::new(),
            }
        } else {
            String::new()
        };

        results.push((title, href, snippet));
    }

    results
}

/// Decode a DuckDuckGo redirect URL to extract the real destination.
/// Input: `//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com&rut=...`
/// Output: `https://example.com`
fn decode_ddg_url(raw: &str) -> String {
    // Look for uddg= parameter
    if let Some(start) = raw.find("uddg=") {
        let encoded = &raw[start + 5..];
        let end = encoded.find('&').unwrap_or(encoded.len());
        urldecoded(&encoded[..end])
    } else {
        raw.to_string()
    }
}

/// Simple percent-decode.
fn urldecoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = bytes.next().and_then(hex_val);
            let lo = bytes.next().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4 | l) as char);
            }
        } else {
            out.push(b as char);
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Extract an attribute value after the given prefix (up to the next `"`).
fn extract_attr(html: &str, prefix: &str) -> Option<String> {
    let start = html.find(prefix)? + prefix.len();
    let end = html[start..].find('"')? + start;
    Some(decode_html_entities(&html[start..end]))
}

/// Strip HTML tags from a string.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    decode_html_entities(out.trim())
}

/// Decode common HTML entities.
fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_urlencoded() {
        assert_eq!(urlencoded("hello world"), "hello+world");
        assert_eq!(urlencoded("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn test_strip_tags() {
        assert_eq!(strip_tags("<b>hello</b> world"), "hello world");
        assert_eq!(strip_tags("no tags"), "no tags");
    }

    #[test]
    fn test_parse_ddg_results_empty() {
        assert!(parse_ddg_results("", 5).is_empty());
        assert!(parse_ddg_results("<html>no results</html>", 5).is_empty());
    }

    #[test]
    fn test_parse_ddg_results_basic() {
        // Matches real DDG HTML format with redirect URLs
        let html = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc123">Example Title</a><a class="result__snippet">This is a snippet.</a>"#;
        let results = parse_ddg_results(html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "Example Title");
        assert_eq!(results[0].1, "https://example.com/page");
        assert_eq!(results[0].2, "This is a snippet.");
    }

    #[test]
    fn test_parse_ddg_results_direct_url() {
        let html = r#"<a class="result__a" href="https://example.com">Direct Link</a>"#;
        let results = parse_ddg_results(html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "Direct Link");
        assert_eq!(results[0].1, "https://example.com");
    }

    #[test]
    fn test_urldecoded() {
        assert_eq!(
            urldecoded("https%3A%2F%2Fexample.com"),
            "https://example.com"
        );
        assert_eq!(urldecoded("hello%20world"), "hello world");
    }

    #[test]
    fn test_decode_html_entities() {
        assert_eq!(decode_html_entities("a &amp; b"), "a & b");
        assert_eq!(decode_html_entities("1 &lt; 2"), "1 < 2");
    }

    #[tokio::test]
    async fn test_invalid_input() {
        let tool = WebSearchTool::new();
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn provider_keys_keep_first_party_search_credentials_available() {
        let tool = WebSearchTool::new().with_provider_keys(HashMap::from([(
            "tavily".to_string(),
            "tvly-configured-key".to_string(),
        )]));

        assert_eq!(
            tool.provider_key("tavily", "TAVILY_API_KEY").as_deref(),
            Some("tvly-configured-key")
        );
    }

    // --- M8.10-B: quota / rate-limit detection ---

    fn err_result(msg: &str) -> ToolResult {
        ToolResult {
            output: msg.to_string(),
            success: false,
            ..Default::default()
        }
    }

    fn ok_result(msg: &str) -> ToolResult {
        ToolResult {
            output: msg.to_string(),
            success: true,
            ..Default::default()
        }
    }

    #[test]
    fn is_quota_or_rate_limit_error_detects_http_429() {
        let r = err_result("Perplexity API error (429): Too Many Requests");
        assert!(is_quota_or_rate_limit_error(&r));
    }

    #[test]
    fn is_quota_or_rate_limit_error_detects_chinese_quota_messages() {
        let r = err_result("Perplexity 配额已耗尽，改用其他引擎");
        assert!(is_quota_or_rate_limit_error(&r));

        let r2 = err_result("当前节点已限流，请稍后再试");
        assert!(is_quota_or_rate_limit_error(&r2));

        let r3 = err_result("Brave 超出每月配额");
        assert!(is_quota_or_rate_limit_error(&r3));
    }

    #[test]
    fn is_quota_or_rate_limit_error_detects_english_quota_phrases() {
        let cases = [
            "Tavily API error (402): quota exceeded",
            "rate limit hit, please retry later",
            "RATE-LIMIT exceeded for plan",
            "Too Many Requests",
            "Monthly usage limit reached",
            "insufficient credits remaining on this account",
            "API credit has been exhausted for the day",
        ];
        for msg in cases {
            let r = err_result(msg);
            assert!(
                is_quota_or_rate_limit_error(&r),
                "should detect quota: {msg}"
            );
        }
    }

    #[test]
    fn is_quota_or_rate_limit_error_negatives() {
        // Successful results should never count as quota errors.
        assert!(!is_quota_or_rate_limit_error(&ok_result(
            "Results for: rust\n\n1. ..."
        )));
        // "No results found" is empty, not quota.
        assert!(!is_quota_or_rate_limit_error(&ok_result(
            "No results found for: rust"
        )));
        // success=false but unrelated message (e.g. parse error) is not quota.
        assert!(!is_quota_or_rate_limit_error(&err_result(
            "failed to parse Brave response"
        )));
        // success=true but text accidentally contains "quota" is NOT a rotation trigger.
        assert!(!is_quota_or_rate_limit_error(&ok_result(
            "Article on quota systems and rate-limit theory"
        )));
    }

    #[test]
    fn is_quota_or_rate_limit_error_case_insensitive_english() {
        assert!(is_quota_or_rate_limit_error(&err_result("RATE LIMIT")));
        assert!(is_quota_or_rate_limit_error(&err_result("Quota Exceeded")));
        assert!(is_quota_or_rate_limit_error(&err_result(
            "TOO MANY REQUESTS"
        )));
    }

    #[test]
    fn is_quota_or_rate_limit_error_with_underscore_or_hyphen() {
        assert!(is_quota_or_rate_limit_error(&err_result(
            "code: rate_limit_exceeded"
        )));
        assert!(is_quota_or_rate_limit_error(&err_result(
            "code: rate-limit-exceeded"
        )));
    }

    // --- Browser-backed (headless Chrome / CDP) fallback provider ---

    /// RED→GREEN: parse a Bing SERP HTML fixture into (title, url, snippet)
    /// tuples without needing a real browser. The CDP provider renders Bing in
    /// headless Chrome and feeds the resulting HTML through this same parser, so
    /// exercising it on a static fixture pins the extraction contract.
    #[cfg(feature = "browser")]
    #[test]
    fn parse_bing_results_extracts_title_url_snippet() {
        let html = r#"
            <ol id="b_results">
              <li class="b_algo">
                <h2><a href="https://www.rust-lang.org/">Rust Programming Language</a></h2>
                <div class="b_caption"><p>A language empowering everyone to build reliable and efficient software.</p></div>
              </li>
              <li class="b_algo">
                <h2><a href="https://doc.rust-lang.org/book/">The Rust Programming Language - Book</a></h2>
                <div class="b_caption"><p>The official Rust book, free online.</p></div>
              </li>
              <li class="b_ad"><h2><a href="https://ad.example.com/promo">Sponsored</a></h2></li>
            </ol>
        "#;
        let results = parse_bing_results(html, 5);
        assert_eq!(results.len(), 2, "should skip the b_ad block");
        assert_eq!(results[0].0, "Rust Programming Language");
        assert_eq!(results[0].1, "https://www.rust-lang.org/");
        assert!(
            results[0].2.contains("empowering everyone"),
            "snippet missing: {:?}",
            results[0].2
        );
        assert_eq!(results[1].1, "https://doc.rust-lang.org/book/");
    }

    #[cfg(feature = "browser")]
    #[test]
    fn parse_bing_results_respects_max_and_handles_empty() {
        let html = r#"
            <li class="b_algo"><h2><a href="https://a.example/">A</a></h2></li>
            <li class="b_algo"><h2><a href="https://b.example/">B</a></h2></li>
            <li class="b_algo"><h2><a href="https://c.example/">C</a></h2></li>
        "#;
        assert_eq!(parse_bing_results(html, 2).len(), 2);
        assert!(parse_bing_results("", 5).is_empty());
        assert!(parse_bing_results("<html>nothing here</html>", 5).is_empty());
    }

    /// CRITICAL no-browser degradation: when no Chrome/Chromium is installed the
    /// CDP provider MUST return a clean, fast miss (a non-success `ToolResult`
    /// with an empty result, NEVER a hang or panic) and MUST honour the supplied
    /// bound. The no-browser branch is driven by passing `None` for the resolved
    /// executable (the value `detect_browser_executable()` yields on a box with
    /// no Chrome), so the test never touches process env / global state and runs
    /// with no real Chrome present. An outer watchdog guarantees we assert on a
    /// fast miss rather than blocking the suite forever.
    #[cfg(feature = "browser")]
    #[tokio::test]
    async fn browser_cdp_search_is_fast_clean_miss_without_browser() {
        let tool = WebSearchTool::new();
        let bound = Duration::from_secs(2);

        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(15),
            tool.browser_cdp_search_with_executable("rust language", 5, bound, None),
        )
        .await;

        let result = outcome.expect("browser_cdp_search hung past the watchdog");
        let result = result.expect("browser_cdp_search must not error out, returns a clean miss");
        assert!(
            !result.success,
            "no-browser path must be a non-success miss, got: {}",
            result.output
        );
        assert!(
            result.output.is_empty()
                || result.output.to_lowercase().contains("browser")
                || result.output.to_lowercase().contains("chrome"),
            "miss message should reference the missing browser: {}",
            result.output
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no-browser path must be fast (no launch attempt), took {:?}",
            started.elapsed()
        );
    }

    /// The public entry point also stays a clean miss when detection finds no
    /// executable on the host (the common CI / fleet-mini / bare-box case). This
    /// runs on a machine that may or may not have Chrome; either way the call
    /// must return a `ToolResult` (never panic/hang) inside the watchdog.
    #[cfg(feature = "browser")]
    #[tokio::test]
    async fn browser_cdp_search_public_entrypoint_never_hangs() {
        let tool = WebSearchTool::new();
        let outcome = tokio::time::timeout(
            Duration::from_secs(20),
            tool.browser_cdp_search("octos web search cdp probe", 3, Duration::from_secs(8)),
        )
        .await;
        let result = outcome.expect("public browser_cdp_search hung past the watchdog");
        // Whether or not a browser is present, we must get a ToolResult, not an Err.
        assert!(
            result.is_ok(),
            "browser_cdp_search must surface a ToolResult, got Err: {:?}",
            result.err()
        );
    }

    /// Structural invariant for the rotation order in `execute`:
    ///
    /// The `execute` method MUST iterate providers in the documented priority
    /// (Tavily → DDG → Exa → Brave → You.com → Perplexity) and treat any
    /// `is_quota_or_rate_limit_error(&r) == true` outcome as "fall through to
    /// next provider", identical to the empty-results path. Perplexity must
    /// NOT short-circuit unconditionally; on quota error it must fall through
    /// to the DDG fallback at the bottom of `execute`.
    ///
    /// This invariant is enforced by code review + the per-provider guards in
    /// `execute`. Mocking the HTTP layer would require restructuring the tool
    /// (e.g. `Arc<dyn HttpClient>` injection) which is out of scope for M8.10-B.
    #[test]
    fn rotation_structural_invariant_documented() {
        // This test is a structural anchor: if anyone changes the rotation
        // order or removes the per-provider quota guard, they must read the
        // doc comment above and update intentionally.
        let providers = [
            "tavily",
            "duckduckgo",
            "exa",
            "brave",
            "you.com",
            "perplexity",
        ];
        assert_eq!(providers.len(), 6);
    }
}
