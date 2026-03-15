//! Web search tool with multiple provider support.
//!
//! Provider priority (best quality first, DDG as free fallback):
//! 1. Exa (`EXA_API_KEY`) — neural/semantic search, 1k free/month
//! 2. DuckDuckGo (no key) — free HTML search
//! 3. Brave Search (`BRAVE_API_KEY`) — free tier: 2k queries/month
//! 4. You.com (`YDC_API_KEY`) — rich JSON results with snippets
//! 5. Perplexity Sonar (`PERPLEXITY_API_KEY`) — AI-synthesized fallback (most expensive)
//!
//! Each provider is tried in order. If a provider returns no results or fails,
//! the next one is attempted. Perplexity is last because it costs the most but
//! gives the best answers (AI-synthesized with citations).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use reqwest::Client;
use serde::Deserialize;
use tracing::info;

use super::{Tool, ToolResult};

pub struct WebSearchTool {
    client: Client,
    config: Option<Arc<super::tool_config::ToolConfigStore>>,
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
        }
    }

    pub fn with_config(mut self, config: Arc<super::tool_config::ToolConfigStore>) -> Self {
        self.config = Some(config);
        self
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

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for information. Supports Exa, Brave, You.com, Perplexity, and DuckDuckGo (auto-detected from environment)."
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

        // Provider priority: free/cheap first, Perplexity as AI-powered fallback.
        // 1. DuckDuckGo (free, always available)
        // 2. Brave Search (free tier: 2k queries/month)
        // 3. You.com (API key required)
        // 4. Perplexity Sonar (AI-synthesized, most expensive — fallback only)

        // Try DuckDuckGo first (free, no key needed)
        let ddg_result = self.ddg_search(&input.query, count).await;
        if let Ok(ref r) = ddg_result {
            if r.success && !r.output.contains("No results found") {
                info!(provider = "duckduckgo", query = %input.query, "web search");
                return ddg_result;
            }
        }

        // Exa (neural search — best for niche/recent topics)
        if let Ok(api_key) = std::env::var("EXA_API_KEY") {
            let result = self.exa_search(&input.query, count, &api_key).await;
            if let Ok(ref r) = result {
                if r.success && !r.output.contains("No results found") {
                    info!(provider = "exa", query = %input.query, "web search");
                    return result;
                }
            }
        }

        // Brave Search
        if let Ok(api_key) = std::env::var("BRAVE_API_KEY") {
            let result = self.brave_search(&input.query, count, &api_key).await;
            if let Ok(ref r) = result {
                if r.success && !r.output.contains("No results found") {
                    info!(provider = "brave", query = %input.query, "web search");
                    return result;
                }
            }
        }

        // You.com
        if let Ok(api_key) = std::env::var("YDC_API_KEY") {
            let result = self.you_search(&input.query, count, &api_key).await;
            if let Ok(ref r) = result {
                if r.success && !r.output.contains("No results found") {
                    info!(provider = "you.com", query = %input.query, "web search");
                    return result;
                }
            }
        }

        // Perplexity Sonar as last resort (AI-synthesized, costs money)
        if let Ok(api_key) = std::env::var("PERPLEXITY_API_KEY") {
            info!(provider = "perplexity", query = %input.query, "web search");
            return self.perplexity_search(&input.query, &api_key).await;
        }

        info!(provider = "duckduckgo (fallback)", query = %input.query, "web search");
        // Return whatever DDG gave us (even if empty)
        ddg_result
    }
}

impl WebSearchTool {
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
                let truncated = if snippet.len() > 300 {
                    format!("{}...", &snippet[..300])
                } else {
                    snippet.clone()
                };
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
}
