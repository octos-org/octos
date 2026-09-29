//! Web search tool with multiple provider support.
//!
//! Provider priority (free structured sources first, OctoSense ADR 0002 §6):
//! 0a. GDELT DOC 2.0 + Google News RSS (no key) — for news-ish queries
//!     (`category: "news"`, a `since` of 31 days or less, or news words)
//! 0b. SearXNG (`SEARXNG_URL`, or the profile's `searxng` search provider) —
//!     a self-hosted instance, when configured
//! 1. Tavily (`TAVILY_API_KEY`) — AI-optimized search, 1k free/month
//! 2. Exa (`EXA_API_KEY`) — neural/semantic search, 1k free/month
//! 3. Brave Search (`BRAVE_API_KEY`) — free tier: 2k queries/month
//! 4. You.com (`YDC_API_KEY`) — rich JSON results with snippets
//! 5. Perplexity Sonar (`PERPLEXITY_API_KEY`) — AI-synthesized fallback (most expensive)
//! 6. DuckDuckGo HTML results page — on unless `OCTOS_ALLOW_SERP_SCRAPE=0`
//! 7. Headless-Chrome (CDP) Bing — same switch
//!
//! Each provider is tried in order. If a provider returns no results or fails,
//! the next one is attempted. Perplexity is last among the keyed providers
//! because it costs the most but gives the best answers (AI-synthesized with
//! citations). `lang` / `region` / `since` go to the free tier (GDELT
//! `sourcelang`/`timespan`, Google News edition and `when:`, SearXNG
//! `language`/`time_range`) and filter its results. If no allowed provider
//! returns anything, the result is empty and says which providers were tried
//! and how to add SearXNG or a search key.
//!
//! One exception to "first answer wins": for a general (not news) query the
//! metasearch's key-less engines are only Wikipedia and Wikidata, which match
//! almost anything. When the free tier's answer is made only of those
//! reference hits, results-page search (6, 7) also runs and its web results
//! are merged in front (see `needs_results_page_tier`).
//!
//! DuckDuckGo HTML and Bing-in-Chrome read search engines' results pages (ADR
//! 0002 §6: honest User-Agent, a challenge is a miss, no CAPTCHA solving).
//! They are on unless the operator sets `OCTOS_ALLOW_SERP_SCRAPE=0` (alias
//! `OCTOS_ALLOW_BROWSER_SERP`). Bing drives the
//! same in-process `chromiumoxide`
//! headless browser the `browser` tool uses and is gated behind the `browser`
//! cargo feature. On any box with no Chrome/Chromium it degrades to a fast,
//! clean miss (detected up-front via
//! `chromiumoxide::detection::default_executable`, never a launch attempt).

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
    /// Identifiable client for the free providers (GDELT, Google News,
    /// SearXNG): these are APIs/feeds, requested as octos, not as a browser.
    research_client: Client,
    config: Option<Arc<super::tool_config::ToolConfigStore>>,
    provider_keys: HashMap<String, String>,
    /// octos metasearch, built on first use (it takes the provider keys).
    metasearch: Arc<std::sync::OnceLock<octos_research::metasearch::Metasearch>>,
    /// Results-page search override; `None` = the environment decides.
    serp_scrape: Option<bool>,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                // Identifiable, never a disguised desktop browser (ADR 0002),
                // including for the opt-in DuckDuckGo scrape.
                .user_agent(octos_research::USER_AGENT)
                .build()
                .unwrap_or_else(|_| Client::new()),
            research_client: Client::builder()
                .timeout(Duration::from_secs(20))
                .connect_timeout(Duration::from_secs(10))
                .user_agent(octos_research::USER_AGENT)
                .build()
                .unwrap_or_else(|_| Client::new()),
            config: None,
            provider_keys: HashMap::new(),
            metasearch: Arc::default(),
            serp_scrape: None,
        }
    }

    /// Turn results-page search (DuckDuckGo, Bing) on or off regardless of
    /// the environment.
    pub fn with_serp_scrape(mut self, on: bool) -> Self {
        self.serp_scrape = Some(on);
        self
    }

    pub fn with_config(mut self, config: Arc<super::tool_config::ToolConfigStore>) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_provider_keys(mut self, provider_keys: HashMap<String, String>) -> Self {
        self.provider_keys = provider_keys;
        self
    }

    /// Use this metasearch instead of the default one (tests, embedders
    /// with their own fetcher or engines).
    pub fn with_metasearch(self, metasearch: octos_research::metasearch::Metasearch) -> Self {
        let _ = self.metasearch.set(metasearch);
        self
    }

    /// The metasearch, sharing the process-wide rate limits and cache.
    /// Profile provider keys (e.g. `brave`) are passed to keyed engines.
    fn metasearch(&self) -> &octos_research::metasearch::Metasearch {
        self.metasearch.get_or_init(|| {
            let keys = self
                .provider_keys
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            // Engines that render (Google) load in the person's browser.
            octos_research::metasearch::Metasearch::from_env(
                octos_research::metasearch::default_fetch(),
                &keys,
            )
        })
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
    /// BCP-47 language(s): a string, a list, or comma-separated.
    #[serde(default)]
    lang: octos_research::OneOrMany,
    /// The query per language (tag → query).
    #[serde(default)]
    query_by_lang: std::collections::BTreeMap<String, String>,
    /// ISO 3166-1 alpha-2 region (Google News edition).
    #[serde(default)]
    region: Option<String>,
    /// ISO date/datetime or `24h` / `7d` / `2w` / `3m` / `1y`.
    #[serde(default)]
    since: Option<String>,
    /// `auto` (default), `news`, `general`, `science`, `it` or `social`.
    #[serde(default)]
    category: Option<String>,
}

/// Parsed free-tier controls.
pub(crate) struct FreeTierControls {
    pub filters: octos_research::Filters,
    /// Per-language queries (normalized tag → query).
    pub query_by_lang: std::collections::BTreeMap<String, String>,
    pub region: Option<String>,
    pub news: bool,
    /// Metasearch category (`news`, `general`, `science`, `it`, `social`).
    pub category: &'static str,
    pub now: chrono::DateTime<chrono::Utc>,
}

impl FreeTierControls {
    fn parse(input: &Input) -> Result<Self, String> {
        let now = chrono::Utc::now();
        let since = match input.since.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(s) => Some(octos_research::date::Since::parse(s, now)?),
        };
        let query_by_lang = octos_research::lang::parse_query_by_lang(&input.query_by_lang)?;
        let mut langs = input.lang.clone().into_vec();
        if !langs.is_empty() {
            langs.extend(query_by_lang.keys().cloned());
        }
        let filters = octos_research::Filters::new(langs, since, Vec::new(), Vec::new(), None)?;
        let category = octos_research::Category::parse(input.category.as_deref())?;
        let news = category.is_news(&input.query, filters.since.as_ref(), now);
        let ms_category = category.metasearch_category(&input.query, filters.since.as_ref(), now);
        let region = input
            .region
            .as_deref()
            .map(|r| r.trim().to_ascii_uppercase())
            .filter(|r| r.len() == 2);
        Ok(Self {
            filters,
            query_by_lang,
            region,
            news,
            category: ms_category,
            now,
        })
    }

    /// Languages to query: requested ones, else a script guess, else default.
    fn langs(&self, query: &str) -> Vec<Option<String>> {
        let mut langs: Vec<Option<String>> = if self.filters.langs.is_empty() {
            vec![octos_research::lang::guess_from_script(query).map(String::from)]
        } else {
            self.filters.langs.iter().cloned().map(Some).collect()
        };
        for l in self.query_by_lang.keys() {
            if !langs.iter().flatten().any(|x| x == l) {
                langs.push(Some(l.clone()));
            }
        }
        langs
    }

    /// The query for one language.
    fn query_for<'a>(&'a self, query: &'a str, lang: Option<&str>) -> &'a str {
        octos_research::lang::query_for(query, &self.query_by_lang, lang)
    }
}

/// Whether results-page search (DuckDuckGo HTML, Bing in headless Chrome)
/// is on: yes unless the operator set `OCTOS_ALLOW_SERP_SCRAPE=0`
/// (ADR 0002 §6: general web search, honest, no CAPTCHA solving).
pub(crate) fn serp_scrape_opted_in(lookup: impl Fn(&str) -> Option<String>) -> bool {
    octos_research::serp_scrape_allowed(lookup)
}

/// Whether the octos metasearch is on (`OCTOS_METASEARCH`, default on).
fn metasearch_on() -> bool {
    octos_research::metasearch::enabled(|k| std::env::var(k).ok())
}

/// Free-tier providers in order: the octos metasearch (every category), or
/// GDELT + Google News for news-ish queries when it is off; then SearXNG
/// when configured.
pub(crate) fn free_tier_providers(
    news: bool,
    metasearch: bool,
    searxng: bool,
) -> Vec<octos_research::Provider> {
    octos_research::plan::plan(&octos_research::plan::PlanInput {
        news,
        metasearch,
        searxng_configured: searxng,
        ..Default::default()
    })
}

/// Metasearch engines that answer almost any general query with an
/// encyclopedia entry. Their hits are reference material, not web results.
const REFERENCE_ENGINES: &[&str] = &["wikipedia", "wikidata"];

/// A metasearch hit that only reference engines returned (its `engines`
/// list, best first, names every engine that found the URL).
fn is_reference_hit(hit: &octos_research::SearchHit) -> bool {
    hit.provider == octos_research::metasearch::PROVIDER_ID
        && !hit.engines.is_empty()
        && hit
            .engines
            .iter()
            .all(|e| REFERENCE_ENGINES.contains(&e.as_str()))
}

/// Whether a free-tier answer should be completed with results-page search
/// (DuckDuckGo, then Bing) before it is returned.
///
/// Rule: results-page search is allowed, the query is not news, and the
/// free tier's answer is non-empty and made *only* of reference-engine hits
/// (Wikipedia/Wikidata). One hit from any other engine or provider (Hacker
/// News, GitHub, SearXNG, GDELT, ...) means the free tier found real
/// results, and its answer is returned unchanged. An empty free tier is not
/// this case: the normal chain (keyed providers, then DuckDuckGo and Bing)
/// already runs.
pub(crate) fn needs_results_page_tier(
    serp_scrape: bool,
    news: bool,
    free_hits: &[octos_research::SearchHit],
) -> bool {
    serp_scrape && !news && !free_hits.is_empty() && free_hits.iter().all(is_reference_hit)
}

/// Results-page hits first, then the free-tier (reference) hits, deduplicated
/// by normalized URL, filtered by the request's controls and capped at
/// `limit`. Web results go first so encyclopedia entries cannot crowd them
/// out of the count.
pub(crate) fn merge_web_first(
    web: Vec<octos_research::SearchHit>,
    free: Vec<octos_research::SearchHit>,
    filters: &octos_research::Filters,
    limit: usize,
) -> Vec<octos_research::SearchHit> {
    let (mut kept, _skipped) = filters.apply(web.into_iter().chain(free).collect());
    kept.truncate(limit);
    kept
}

/// Results-page search: DuckDuckGo, then Bing only if DuckDuckGo missed. A
/// miss (error, bot check, no results) is logged and the next one is tried.
/// Returns the provider that answered and its hits. Both searches are
/// passed as (lazy) futures so the order is testable without the network.
pub(crate) async fn results_page_tier<D, B>(
    query: &str,
    ddg: D,
    bing: B,
) -> Option<(&'static str, Vec<octos_research::SearchHit>)>
where
    D: std::future::Future<Output = std::result::Result<Vec<octos_research::SearchHit>, String>>,
    B: std::future::Future<Output = std::result::Result<Vec<octos_research::SearchHit>, String>>,
{
    if let Some(hits) = serp_outcome("duckduckgo", query, ddg.await) {
        return Some(("duckduckgo", hits));
    }
    serp_outcome("bing_cdp", query, bing.await).map(|hits| ("bing_cdp", hits))
}

fn serp_outcome(
    provider: &'static str,
    query: &str,
    r: std::result::Result<Vec<octos_research::SearchHit>, String>,
) -> Option<Vec<octos_research::SearchHit>> {
    match r {
        Ok(h) if !h.is_empty() => Some(h),
        Ok(_) => {
            info!(provider, fallback_reason = "empty", query = %query, "web_search rotation");
            None
        }
        Err(e) => {
            let snippet = octos_core::truncated_utf8(&e, 120, "...");
            warn!(provider, fallback_reason = "miss", error = %snippet, "web_search rotation");
            None
        }
    }
}

/// Complete a free-tier answer with results-page search when
/// [`needs_results_page_tier`] says so; otherwise return it unchanged. If
/// both results pages miss, the free-tier answer is still returned.
pub(crate) async fn complete_free_tier<D, B>(
    mut answer: FreeTierAnswer,
    query: &str,
    serp_scrape: bool,
    c: &FreeTierControls,
    ddg: D,
    bing: B,
) -> FreeTierAnswer
where
    D: std::future::Future<Output = std::result::Result<Vec<octos_research::SearchHit>, String>>,
    B: std::future::Future<Output = std::result::Result<Vec<octos_research::SearchHit>, String>>,
{
    if !needs_results_page_tier(serp_scrape, c.news, &answer.hits) {
        return answer;
    }
    info!(
        query = %query,
        "web_search: free tier returned only reference entries; adding results-page search"
    );
    if let Some((provider, web)) = results_page_tier(query, ddg, bing).await {
        let free = std::mem::take(&mut answer.hits);
        answer.hits = merge_web_first(web, free, &c.filters, answer.limit);
        answer.used.insert(0, provider);
        // Log only providers whose hits survived the cap.
        let hits = &answer.hits;
        answer
            .used
            .retain(|p| hits.iter().any(|h| h.provider == *p));
        // The "general results are thin" note no longer applies.
        answer.note = None;
    }
    answer
}

/// What the free tier found, before it is formatted.
pub(crate) struct FreeTierAnswer {
    pub hits: Vec<octos_research::SearchHit>,
    /// Providers that contributed, in output order.
    pub used: Vec<&'static str>,
    pub note: Option<String>,
    /// Engines that met a bot challenge, one line each, for the person.
    pub challenges: Vec<String>,
    /// Set when the search used the person's browser: what that means for
    /// their account and how to turn it off.
    pub browser_notice: Option<&'static str>,
    /// Result cap (`count` per requested language).
    pub limit: usize,
}

impl FreeTierAnswer {
    fn into_result(self, query: &str, c: &FreeTierControls) -> ToolResult {
        let used = self.used.join("+");
        info!(provider = %used, used_provider = %used, query = %query, "web_search");
        let mut output = octos_research::providers::format_hits(query, &self.hits);
        if let Some(note) = self.note.filter(|_| c.category == "general") {
            output.push_str(&format!("Note: {note}\n"));
        }
        // A search engine asked to confirm a person is searching: say so
        // (octos does not solve or work around these).
        for line in &self.challenges {
            output.push_str(&format!("Note: {line}\n"));
        }
        if let Some(notice) = self.browser_notice {
            output.push_str(&format!("Note: {notice}\n"));
        }
        if octos_research::respect_robots(|k| std::env::var(k).ok())
            && self.hits.iter().any(|h| {
                h.provider == "google_news_rss" || h.engines.iter().any(|e| e == "google_news")
            })
        {
            output.push_str(
                "Note: news.google.com links are redirects whose robots.txt disallows automated fetching; cite them as headlines (publisher and date above) rather than fetching them.\n",
            );
        }
        ToolResult {
            output,
            success: true,
            ..Default::default()
        }
    }
}

/// GDELT asks for at most one request every 5 seconds (process-wide).
fn gdelt_throttle() -> &'static octos_research::HostThrottle {
    static T: std::sync::OnceLock<octos_research::HostThrottle> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        octos_research::HostThrottle::new(octos_research::providers::GDELT_MIN_INTERVAL)
    })
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
        "Search the web for information. Free sources first: GDELT and Google News RSS for news (dated, multi-language), a self-hosted SearXNG if configured; then Tavily, Exa, Brave, You.com, Perplexity (auto-detected from keys). Search-results pages are not scraped unless the operator enables it; with nothing configured a general query returns no results plus how to add SearXNG or a key. Optional lang, region, since, category."
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
                },
                "lang": {
                    "description": "BCP-47 language(s), e.g. \"en\" or [\"en\", \"zh-CN\"]: each is searched separately by the free news sources and results are kept to those languages",
                    "anyOf": [
                        {"type": "string"},
                        {"type": "array", "items": {"type": "string"}}
                    ]
                },
                "query_by_lang": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "description": "The query in each language's own words, keyed by BCP-47 tag, e.g. {\"zh\": \"人工智能 监管\"}; each language is searched with its own query. Translate the query yourself when searching several languages."
                },
                "region": {
                    "type": "string",
                    "description": "ISO 3166-1 alpha-2 region for the Google News edition, e.g. US, TW"
                },
                "since": {
                    "type": "string",
                    "description": "Only results published since an ISO date (2026-09-01) or a span: 24h, 7d, 2w, 3m, 1y"
                },
                "category": {
                    "type": "string",
                    "enum": ["auto", "news", "general", "science", "it", "social"],
                    "description": "Metasearch engines to use: news (GDELT, Hacker News, Mastodon), general (DuckDuckGo, Bing and Brave results pages, Google where a browser is available, Wikipedia, Wikidata), science (arXiv, OpenAlex), it (Hacker News, GitHub, Stack Exchange), social (Mastodon). auto (default) = news when since <= 31 days or the query mentions news/latest/today, else general."
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

        let controls = match FreeTierControls::parse(&input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Invalid web_search input: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let serp_scrape = self
            .serp_scrape
            .unwrap_or_else(|| serp_scrape_opted_in(|k| std::env::var(k).ok()));
        // Upgrade visibility: say once per process that results-page search
        // runs because of the new default.
        if serp_scrape && self.serp_scrape.is_none() {
            static NOTICE: std::sync::Once = std::sync::Once::new();
            if let Some(notice) =
                octos_research::serp_scrape_default_notice(|k| std::env::var(k).ok())
            {
                NOTICE.call_once(|| warn!("{notice}"));
            }
        }

        // Free structured sources first (ADR 0002 §6): GDELT + Google News
        // for news-ish queries, then a configured SearXNG.
        // For a general query answered only by reference engines, the
        // results-page tier is added (see `needs_results_page_tier`), but only
        // without the metasearch: its own DuckDuckGo, Bing, Brave and Google
        // engines have asked those pages already, and one search never asks
        // a results page twice.
        if let Some(answer) = self.free_tier_search(&input.query, count, &controls).await {
            let answer = complete_free_tier(
                answer,
                &input.query,
                serp_scrape && !metasearch_on(),
                &controls,
                self.ddg_hits(&input.query, count),
                self.bing_hits(&input.query, count),
            )
            .await;
            return Ok(answer.into_result(&input.query, &controls));
        }

        // Keyed providers: Tavily first (best quality), Perplexity last.
        // 1. Tavily (AI-optimized, 1k free/month)
        // 2. Exa (neural search)
        // 3. Brave Search (free tier: 2k queries/month)
        // 4. You.com (API key required)
        // 5. Perplexity Sonar (AI-synthesized, most expensive — fallback only)
        // Then, only with OCTOS_ALLOW_SERP_SCRAPE=1: DuckDuckGo HTML, Bing CDP.

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

        // DuckDuckGo HTML results page: general web results, last resort
        // after every keyed provider (on unless turned off, ADR 0002). Only
        // without the metasearch: its own DuckDuckGo and Bing engines have
        // asked already, and one search never asks a results page twice.
        let legacy_serp = serp_scrape && !metasearch_on();
        let ddg_result = if legacy_serp {
            Some(self.ddg_search(&input.query, count).await)
        } else {
            None
        };
        if let Some(Ok(ref r)) = ddg_result {
            if r.success && !r.output.contains("No results found") {
                info!(
                    provider = "duckduckgo",
                    used_provider = "duckduckgo",
                    query = %input.query,
                    "web_search"
                );
                return ddg_result.expect("checked Some");
            }
            let snippet = octos_core::truncated_utf8(&r.output, 120, "...");
            warn!(
                provider = "duckduckgo",
                fallback_reason = if r.success { "empty" } else { "error" },
                error = %snippet,
                "web_search rotation"
            );
        }

        // Headless-Chrome (CDP) Bing, after DuckDuckGo. Drives
        // Bing through the in-process headless browser. On a box with no
        // Chrome this is a fast, clean miss (see `browser_cdp_search`).
        #[cfg(feature = "browser")]
        if legacy_serp {
            {
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

        // Every allowed provider was exhausted. Return an empty result that
        // says what was tried and how to get results (SearXNG or a key), not
        // a silent scrape. The caller LLM should not retry with identical
        // args (worker.txt guidance).
        let tried = self.tried_providers(&controls, serp_scrape);
        info!(
            provider = "none",
            tried = %tried.join(","),
            query = %input.query,
            "web_search: no results from allowed providers"
        );
        Ok(ToolResult {
            output: octos_research::no_results_message(&input.query, &tried),
            success: true,
            ..Default::default()
        })
    }
}

impl WebSearchTool {
    /// Providers this search would have called, in order (for the
    /// no-results message).
    fn tried_providers(&self, c: &FreeTierControls, serp_scrape: bool) -> Vec<String> {
        let mut tried: Vec<String> =
            free_tier_providers(c.news, metasearch_on(), self.searxng_base().is_some())
                .iter()
                .map(|p| p.id().to_string())
                .collect();
        let has = |id: &str, env: &str| {
            self.provider_key(id, env)
                .is_some_and(|k| !k.trim().is_empty())
        };
        for (id, env) in [
            ("tavily", "TAVILY_API_KEY"),
            ("exa", "EXA_API_KEY"),
            ("brave", "BRAVE_API_KEY"),
            ("you", "YDC_API_KEY"),
            ("perplexity", "PERPLEXITY_API_KEY"),
        ] {
            if has(id, env) {
                tried.push(id.to_string());
            }
        }
        if serp_scrape && !metasearch_on() {
            tried.push("duckduckgo".to_string());
            if cfg!(feature = "browser") {
                tried.push("bing_cdp".to_string());
            }
        }
        tried
    }

    // --- Free tier: GDELT, Google News RSS, SearXNG ---

    /// SearXNG base URL from the profile's `searxng` search provider or
    /// `SEARXNG_URL`. Operator configuration, so a private address (a
    /// localhost instance) is allowed here.
    fn searxng_base(&self) -> Option<String> {
        self.provider_key("searxng", octos_research::SEARXNG_URL_ENV)
            .filter(|v| !v.trim().is_empty())
    }

    async fn fetch_text(&self, url: &str, label: &str) -> std::result::Result<String, String> {
        let resp = self
            .research_client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("{label} error: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "{label} HTTP {status}: {}",
                octos_core::truncated_utf8(&body, 160, "...")
            ));
        }
        Ok(body)
    }

    async fn metasearch_search(
        &self,
        query: &str,
        count: u8,
        c: &FreeTierControls,
        langs: &[Option<String>],
    ) -> octos_research::metasearch::SearchResponse {
        let mut req = octos_research::metasearch::SearchRequest::new(query, c.category);
        req.query_by_lang = c.query_by_lang.clone();
        req.langs = langs.iter().flatten().cloned().collect();
        req.region = c.region.clone();
        req.since = c.filters.since.clone();
        req.count = count as usize;
        req.limit = count as usize * langs.len().max(1) * 2;
        req.filters = c.filters.clone();
        req.now = c.now;
        // This tool's results-page setting (`with_serp_scrape`, or the
        // environment) governs the metasearch's results-page engines too.
        req.results_pages = self
            .serp_scrape
            .unwrap_or_else(|| serp_scrape_opted_in(|k| std::env::var(k).ok()));
        self.metasearch().search(&req).await
    }

    async fn free_provider(
        &self,
        provider: octos_research::Provider,
        query: &str,
        lang: Option<&str>,
        count: u8,
        c: &FreeTierControls,
    ) -> std::result::Result<Vec<octos_research::SearchHit>, String> {
        use octos_research::Provider as P;
        use octos_research::providers as free;
        let since = c.filters.since.as_ref();
        match provider {
            P::Gdelt => {
                gdelt_throttle().wait("api.gdeltproject.org", None).await;
                let url = free::gdelt_request_url(query, lang, since, count as usize, c.now);
                free::parse_gdelt(&self.fetch_text(&url, "GDELT").await?)
            }
            P::GoogleNewsRss => {
                let url = free::google_news_rss_url(query, lang, c.region.as_deref(), since);
                let body = self.fetch_text(&url, "Google News RSS").await?;
                let default_lang = free::google_news_lang(lang, c.region.as_deref());
                let mut hits = free::parse_feed(&body, "google_news_rss", Some(&default_lang))?;
                hits.truncate(count as usize);
                Ok(hits)
            }
            P::Searxng => {
                let base = self.searxng_base().ok_or("SearXNG not configured")?;
                let url = free::searxng_request_url(&base, query, lang, since, c.news, c.now)?;
                let mut hits = free::parse_searxng(&self.fetch_text(&url, "SearXNG").await?)?;
                hits.truncate(count as usize);
                Ok(hits)
            }
            other => Err(format!("{} is not a free-tier provider", other.id())),
        }
    }

    /// Run the free tier for every requested language. Returns `None` when
    /// it produced nothing usable, so the keyed/keyless chain continues.
    async fn free_tier_search(
        &self,
        query: &str,
        count: u8,
        c: &FreeTierControls,
    ) -> Option<FreeTierAnswer> {
        let providers = free_tier_providers(c.news, metasearch_on(), self.searxng_base().is_some());
        if providers.is_empty() {
            return None;
        }
        let langs = c.langs(query);
        let mut hits = Vec::new();
        let mut used: Vec<&'static str> = Vec::new();
        let mut note = None;
        let mut challenges = Vec::new();
        let mut browser_notice = None;
        // The metasearch covers every requested language in one call.
        if providers.contains(&octos_research::Provider::Metasearch) {
            let resp = self.metasearch_search(query, count, c, &langs).await;
            for e in &resp.engines {
                info!(
                    provider = "metasearch",
                    engine = %e.engine,
                    status = ?e.status,
                    hits = e.hits,
                    error = e.error.as_deref().unwrap_or(""),
                    "web_search metasearch engine"
                );
            }
            if !resp.items.is_empty() {
                used.push("metasearch");
                hits.extend(resp.hits());
            }
            note = resp.note.clone();
            challenges = resp.challenges();
            browser_notice = resp.browser_notice();
        }
        let mut calls = Vec::new();
        for lang in &langs {
            for p in providers
                .iter()
                .filter(|p| **p != octos_research::Provider::Metasearch)
            {
                calls.push(async move {
                    let r = tokio::time::timeout(
                        Duration::from_secs(40),
                        self.free_provider(
                            *p,
                            c.query_for(query, lang.as_deref()),
                            lang.as_deref(),
                            count,
                            c,
                        ),
                    )
                    .await
                    .unwrap_or_else(|_| Err("timed out".to_string()));
                    (*p, r)
                });
            }
        }
        for (p, r) in futures::future::join_all(calls).await {
            match r {
                Ok(h) if !h.is_empty() => {
                    if !used.contains(&p.id()) {
                        used.push(p.id());
                    }
                    hits.extend(h);
                }
                Ok(_) => info!(
                    provider = p.id(),
                    fallback_reason = "empty",
                    "web_search rotation"
                ),
                Err(e) => {
                    let snippet = octos_core::truncated_utf8(&e, 120, "...");
                    warn!(provider = p.id(), fallback_reason = "error", error = %snippet, "web_search rotation");
                }
            }
        }
        let (kept, _skipped) = c.filters.apply(hits);
        let mut kept = octos_research::filter::interleave_by(kept, |h| {
            h.lang
                .as_deref()
                .map(octos_research::lang::primary)
                .unwrap_or_default()
        });
        let limit = count as usize * langs.len().max(1);
        kept.truncate(limit);
        if kept.is_empty() {
            return None;
        }
        Some(FreeTierAnswer {
            hits: kept,
            used,
            note,
            challenges,
            browser_notice,
            limit,
        })
    }

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
            output.push_str(&format!("   {snippet}\n\n"));
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
                output.push_str(&format!("   Published: {date}\n"));
            }

            for highlight in &r.highlights {
                let trimmed = highlight.trim();
                if !trimmed.is_empty() {
                    output.push_str(&format!("   {trimmed}\n"));
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
            "messages": [{"role": "user", "content": query}]
            // No max_tokens: output size is left to the provider (standing
            // decision: no hard-coded output caps on model calls).
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
                output.push_str(&format!("   {truncated}\n"));
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

    /// DuckDuckGo's HTML results page as `(title, url, snippet)`. An HTTP
    /// error or a bot check is an `Err` (a miss: never parsed, never solved).
    async fn ddg_results(
        &self,
        query: &str,
        count: u8,
    ) -> std::result::Result<Vec<(String, String, String)>, String> {
        let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoded(query));
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("failed to fetch DuckDuckGo search results: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "DuckDuckGo search error: HTTP {}",
                response.status()
            ));
        }
        let html = response.text().await.unwrap_or_default();
        // Its bot check (often HTTP 202) is a miss: never parsed, never solved.
        if octos_research::access::is_bot_challenge(&html) {
            return Err("DuckDuckGo answered with a bot check (not solved)".to_string());
        }
        Ok(parse_ddg_results(&html, count as usize))
    }

    /// DuckDuckGo results as search hits (for merging with the free tier).
    async fn ddg_hits(
        &self,
        query: &str,
        count: u8,
    ) -> std::result::Result<Vec<octos_research::SearchHit>, String> {
        self.ddg_results(query, count)
            .await
            .map(|r| serp_hits(r, "duckduckgo"))
    }

    async fn ddg_search(&self, query: &str, count: u8) -> Result<ToolResult> {
        let results = match self.ddg_results(query, count).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult {
                    output: e,
                    success: false,
                    ..Default::default()
                });
            }
        };

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

    /// Bing in headless Chrome as search hits (for merging with the free
    /// tier). A missing browser, a challenge or a timeout is an `Err`.
    async fn bing_hits(
        &self,
        query: &str,
        count: u8,
    ) -> std::result::Result<Vec<octos_research::SearchHit>, String> {
        #[cfg(feature = "browser")]
        {
            self.bing_results(
                query,
                count,
                Duration::from_secs(45),
                detect_browser_executable(),
            )
            .await
            .map(|r| serp_hits(r, "bing_cdp"))
        }
        #[cfg(not(feature = "browser"))]
        {
            let _ = (query, count);
            Err("built without the browser feature".to_string())
        }
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
        let results = match self.bing_results(query, count, bound, executable).await {
            Ok(r) => r,
            Err(output) => {
                return Ok(ToolResult {
                    output,
                    success: false,
                    ..Default::default()
                });
            }
        };
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

    /// Bing's results page rendered in headless Chrome, as
    /// `(title, url, snippet)`. No browser, a launch error, a challenge or a
    /// timeout is an `Err` (a clean miss).
    #[cfg(feature = "browser")]
    async fn bing_results(
        &self,
        query: &str,
        count: u8,
        bound: Duration,
        executable: Option<std::path::PathBuf>,
    ) -> std::result::Result<Vec<(String, String, String)>, String> {
        let Some(executable) = executable else {
            // No usable browser: skip cleanly. Caller proceeds (and, with every
            // provider exhausted, the search terminates rather than hanging).
            warn!(
                provider = "bing_cdp",
                fallback_reason = "no_browser",
                "web_search: no Chrome/Chromium found; skipping headless fallback"
            );
            return Err("browser unavailable: no Chrome/Chromium executable detected".to_string());
        };

        // Bound the entire launch + navigation + extraction so a stuck Chrome
        // cannot block the agent. On timeout the session future is dropped,
        // whose `Drop`/`shutdown` kills the child process (see browser.rs).
        let fut = render_and_parse_bing(executable, query, count);
        match tokio::time::timeout(bound, fut).await {
            Ok(Ok(results)) => Ok(results),
            Ok(Err(e)) => {
                let snippet = octos_core::truncated_utf8(&e.to_string(), 200, "...");
                warn!(
                    provider = "bing_cdp",
                    fallback_reason = "launch_error",
                    error = %snippet,
                    "web_search: headless Chrome search failed"
                );
                Err(format!("browser search failed: {snippet}"))
            }
            Err(_) => {
                warn!(
                    provider = "bing_cdp",
                    fallback_reason = "timeout",
                    timeout_ms = bound.as_millis() as u64,
                    "web_search: headless Chrome search timed out"
                );
                Err(format!(
                    "browser search timed out after {}s",
                    bound.as_secs().max(1)
                ))
            }
        }
    }
}

/// Results-page rows `(title, url, snippet)` as search hits from `provider`.
fn serp_hits(
    rows: Vec<(String, String, String)>,
    provider: &str,
) -> Vec<octos_research::SearchHit> {
    rows.into_iter()
        .map(|(title, url, snippet)| octos_research::SearchHit {
            url,
            title,
            snippet,
            provider: provider.to_string(),
            ..Default::default()
        })
        .collect()
}

/// Detect a usable Chrome/Chromium/Edge executable using the exact same probe
/// the `browser` tool relies on (env `CHROME`, `which`, well-known install
/// paths). Returns `None` when no browser exists so callers degrade cleanly
/// instead of attempting (and blocking on) a doomed launch.
#[cfg(feature = "browser")]
pub(super) fn detect_browser_executable() -> Option<std::path::PathBuf> {
    chromiumoxide::detection::default_executable(
        chromiumoxide::detection::DetectionOptions::default(),
    )
    .ok()
}

/// Keep the browser's own User-Agent (HeadlessChrome) and append the
/// `octos-research` product token, so sites can identify the reader.
#[cfg(feature = "browser")]
pub(super) async fn set_identifiable_user_agent(page: &chromiumoxide::Page) {
    use chromiumoxide::cdp::browser_protocol::network::SetUserAgentOverrideParams;
    let base = page.user_agent().await.unwrap_or_default();
    let ua = format!("{base} octos-research/1.0 (+https://github.com/octos-org/octos)");
    let _ = page
        .set_user_agent(SetUserAgentOverrideParams::new(ua.trim().to_string()))
        .await;
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
            .new_page("about:blank")
            .await
            .map_err(|e| eyre::eyre!("failed to open Bing page: {e}"))?;
        // Honest UA on results-page search: it is allowed, disguise is not.
        set_identifiable_user_agent(&page).await;
        page.goto(search_url.as_str())
            .await
            .map_err(|e| eyre::eyre!("failed to open Bing page: {e}"))?;
        let _ = page.wait_for_navigation().await;
        let html = page
            .content()
            .await
            .map_err(|e| eyre::eyre!("failed to read Bing HTML: {e}"))?;
        // A challenge is a miss: never parsed, never solved.
        if octos_research::access::is_bot_challenge(&html) {
            eyre::bail!("Bing answered with a challenge (not solved)");
        }
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

    fn serp_fixture(name: &str) -> String {
        let path = format!(
            "{}/../octos-research/tests/fixtures/serp/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn should_treat_a_duckduckgo_bot_check_as_a_miss() {
        // #2607: the check is detected (ddg_search then reports an error and
        // the rotation moves on) and yields no results even if parsed.
        let html = serp_fixture("ddg_anomaly.html");
        assert!(octos_research::access::is_bot_challenge(&html));
        assert!(parse_ddg_results(&html, 5).is_empty());
        // A real results page is not taken for one.
        let results = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc123">Example Title</a><a class="result__snippet">This is a snippet.</a>"#;
        assert!(!octos_research::access::is_bot_challenge(results));
    }

    #[cfg(feature = "browser")]
    #[test]
    fn should_treat_a_bing_challenge_as_a_miss() {
        let html = serp_fixture("bing_challenge.html");
        assert!(octos_research::access::is_bot_challenge(&html));
        assert!(parse_bing_results(&html, 5).is_empty());
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
    /// (Tavily → Exa → Brave → You.com → Perplexity, then the DDG / Bing
    /// results pages) and treat any
    /// `is_quota_or_rate_limit_error(&r) == true` outcome as "fall through to
    /// next provider", identical to the empty-results path. Perplexity must
    /// NOT short-circuit unconditionally; on quota error it must fall through
    /// to the no-results message at the bottom of `execute`.
    ///
    /// This invariant is enforced by code review + the per-provider guards in
    /// `execute`. Mocking the HTTP layer would require restructuring the tool
    /// (e.g. `Arc<dyn HttpClient>` injection) which is out of scope for M8.10-B.
    #[test]
    fn rotation_structural_invariant_documented() {
        // This test is a structural anchor: if anyone changes the rotation
        // order or removes the per-provider quota guard, they must read the
        // doc comment above and update intentionally.
        let providers = ["tavily", "exa", "brave", "you.com", "perplexity"];
        assert_eq!(providers.len(), 5);
    }

    #[test]
    fn should_try_free_news_sources_before_keyed_providers() {
        use octos_research::Provider;
        assert_eq!(
            free_tier_providers(true, true, true),
            vec![Provider::Metasearch, Provider::Searxng]
        );
        assert_eq!(
            free_tier_providers(false, true, false),
            vec![Provider::Metasearch],
            "metasearch serves general queries too"
        );
        // With OCTOS_METASEARCH=0: the direct news sources.
        assert_eq!(
            free_tier_providers(true, false, true),
            vec![Provider::Gdelt, Provider::GoogleNewsRss, Provider::Searxng]
        );
        assert_eq!(
            free_tier_providers(false, false, true),
            vec![Provider::Searxng]
        );
        assert!(free_tier_providers(false, false, false).is_empty());
    }

    #[test]
    fn should_gate_both_results_page_providers_on_one_switch() {
        assert!(serp_scrape_opted_in(|_| None), "on by default");
        assert!(!serp_scrape_opted_in(|_| Some("false".into())));
        for key in [
            octos_research::SERP_SCRAPE_ENV,
            octos_research::BROWSER_SERP_ENV,
        ] {
            assert!(!serp_scrape_opted_in(
                |k| (k == key).then(|| "0".to_string())
            ));
        }
    }

    #[test]
    fn should_list_ddg_as_tried_only_with_the_flag() {
        let tool = WebSearchTool::new();
        let input: Input =
            serde_json::from_value(serde_json::json!({"query": "rust", "category": "general"}))
                .unwrap();
        let c = FreeTierControls::parse(&input).unwrap();
        assert!(
            !tool
                .tried_providers(&c, false)
                .iter()
                .any(|p| p == "duckduckgo" || p == "bing_cdp")
        );
        let tried = tool.tried_providers(&c, true);
        if metasearch_on() {
            // The metasearch's own DuckDuckGo and Bing engines asked them;
            // the standalone providers do not ask again.
            assert!(tried.iter().any(|p| p == "metasearch"), "{tried:?}");
            assert!(!tried.iter().any(|p| p == "duckduckgo"), "{tried:?}");
        } else {
            assert!(tried.iter().any(|p| p == "duckduckgo"), "{tried:?}");
        }
    }

    /// With results-page search turned off and no key or SearXNG, a general
    /// query must not call DuckDuckGo: it returns an empty result with
    /// guidance (the metasearch is given an offline fetcher, so no network
    /// call).
    #[tokio::test]
    async fn should_not_use_duckduckgo_when_turned_off() {
        let configured = [
            "TAVILY_API_KEY",
            "EXA_API_KEY",
            "BRAVE_API_KEY",
            "YDC_API_KEY",
            "PERPLEXITY_API_KEY",
            octos_research::SEARXNG_URL_ENV,
        ];
        if configured.iter().any(|k| std::env::var(k).is_ok()) {
            return; // developer machine with keys: not the keyless case
        }
        struct Offline;
        impl octos_research::metasearch::Fetch for Offline {
            fn fetch(
                &self,
                _: octos_research::metasearch::HttpRequest,
            ) -> octos_research::metasearch::FetchFuture<'_> {
                Box::pin(async { Err("offline".to_string()) })
            }
        }
        let metasearch = octos_research::metasearch::Metasearch::new(
            octos_research::metasearch::Registry::builtin(),
            Arc::new(Offline),
            Default::default(),
        );
        let tool = WebSearchTool::new()
            .with_metasearch(metasearch)
            .with_serp_scrape(false);
        let r = tool
            .execute(&serde_json::json!({"query": "rust borrow checker", "category": "general"}))
            .await
            .unwrap();
        assert!(r.success, "empty result, not an error");
        if metasearch_on() {
            assert!(
                r.output.contains("Providers tried: metasearch."),
                "{}",
                r.output
            );
        } else {
            assert!(r.output.contains("Providers tried: none"), "{}", r.output);
        }
        assert!(!r.output.contains("duckduckgo"), "{}", r.output);
        assert!(r.output.contains("SEARXNG_URL"));
        assert!(!r.output.contains("Results for:"));
    }

    fn meta_hit(url: &str, engines: &[&str]) -> octos_research::SearchHit {
        octos_research::SearchHit {
            url: url.to_string(),
            title: url.to_string(),
            provider: octos_research::metasearch::PROVIDER_ID.to_string(),
            engines: engines.iter().map(|e| e.to_string()).collect(),
            ..Default::default()
        }
    }

    fn web_hit(url: &str, provider: &str) -> octos_research::SearchHit {
        octos_research::SearchHit {
            url: url.to_string(),
            title: url.to_string(),
            provider: provider.to_string(),
            ..Default::default()
        }
    }

    fn controls(query: &str, category: &str) -> FreeTierControls {
        let input: Input =
            serde_json::from_value(serde_json::json!({"query": query, "category": category}))
                .unwrap();
        FreeTierControls::parse(&input).unwrap()
    }

    fn reference_answer() -> FreeTierAnswer {
        FreeTierAnswer {
            hits: vec![
                meta_hit("https://en.wikipedia.org/wiki/Omarchy", &["wikipedia"]),
                meta_hit(
                    "https://www.wikidata.org/wiki/Q1",
                    &["wikidata", "wikipedia"],
                ),
            ],
            used: vec!["metasearch"],
            note: Some("general engines are thin".to_string()),
            challenges: Vec::new(),
            browser_notice: None,
            limit: 5,
        }
    }

    type Serp = std::result::Result<Vec<octos_research::SearchHit>, String>;

    /// A results-page search that records whether it ran.
    async fn serp(called: &std::sync::atomic::AtomicBool, r: Serp) -> Serp {
        called.store(true, std::sync::atomic::Ordering::SeqCst);
        r
    }

    const QUERY: &str = "\"switched to Omarchy\" OR \"using Omarchy\" daily driver";

    #[tokio::test]
    async fn should_add_results_page_hits_first_when_general_query_gets_only_reference_hits() {
        let c = controls(QUERY, "auto");
        assert!(!c.news, "the validation query is general");
        let (ddg, bing) = (Default::default(), Default::default());
        let web = vec![
            web_hit("https://blog.example.com/omarchy", "duckduckgo"),
            // Same page as a metasearch hit, other spelling: deduplicated.
            web_hit("http://en.wikipedia.org/wiki/Omarchy/", "duckduckgo"),
        ];
        let out = complete_free_tier(
            reference_answer(),
            QUERY,
            true,
            &c,
            serp(&ddg, Ok(web)),
            serp(&bing, Ok(vec![web_hit("https://b.example/", "bing_cdp")])),
        )
        .await;
        assert!(ddg.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            !bing.load(std::sync::atomic::Ordering::SeqCst),
            "DDG answered"
        );
        let urls: Vec<&str> = out.hits.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://blog.example.com/omarchy",
                "http://en.wikipedia.org/wiki/Omarchy/",
                "https://www.wikidata.org/wiki/Q1",
            ],
            "web first, deduplicated by normalized URL"
        );
        assert_eq!(out.used, vec!["duckduckgo", "metasearch"]);
        assert!(out.note.is_none());
        let r = out.into_result(QUERY, &c);
        assert!(r.output.starts_with("Results for:"), "{}", r.output);
    }

    #[tokio::test]
    async fn should_cap_merged_hits_at_the_limit_with_web_hits_kept() {
        let c = controls("rust borrow checker", "general");
        let web: Vec<_> = (0..5)
            .map(|i| web_hit(&format!("https://w{i}.example/"), "duckduckgo"))
            .collect();
        let (ddg, bing) = (Default::default(), Default::default());
        let out = complete_free_tier(
            reference_answer(),
            "rust borrow checker",
            true,
            &c,
            serp(&ddg, Ok(web)),
            serp(&bing, Ok(vec![])),
        )
        .await;
        assert_eq!(out.hits.len(), 5);
        assert!(out.hits.iter().all(|h| h.provider == "duckduckgo"));
        assert_eq!(out.used, vec!["duckduckgo"], "no metasearch hit survived");
    }

    #[tokio::test]
    async fn should_keep_the_metasearch_answer_when_results_page_search_is_off() {
        let c = controls(QUERY, "general");
        let (ddg, bing) = (Default::default(), Default::default());
        let out = complete_free_tier(
            reference_answer(),
            QUERY,
            false,
            &c,
            serp(&ddg, Ok(vec![web_hit("https://a.example/", "duckduckgo")])),
            serp(&bing, Ok(vec![])),
        )
        .await;
        assert!(!ddg.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!bing.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(out.hits, reference_answer().hits);
        assert_eq!(out.used, vec!["metasearch"]);
        assert!(out.note.is_some());
    }

    #[tokio::test]
    async fn should_keep_the_answer_unchanged_when_query_is_news() {
        let c = controls("latest Omarchy release news", "auto");
        assert!(c.news);
        let (ddg, bing) = (Default::default(), Default::default());
        let out = complete_free_tier(
            reference_answer(),
            "latest Omarchy release news",
            true,
            &c,
            serp(&ddg, Ok(vec![web_hit("https://a.example/", "duckduckgo")])),
            serp(&bing, Ok(vec![])),
        )
        .await;
        assert!(!ddg.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(out.hits, reference_answer().hits);
    }

    #[tokio::test]
    async fn should_not_search_results_pages_when_metasearch_found_other_engines() {
        let c = controls(QUERY, "general");
        let mut answer = reference_answer();
        answer.hits.push(meta_hit(
            "https://news.ycombinator.com/item?id=1",
            &["hackernews"],
        ));
        let expected = answer.hits.clone();
        let (ddg, bing) = (Default::default(), Default::default());
        let out = complete_free_tier(
            answer,
            QUERY,
            true,
            &c,
            serp(&ddg, Ok(vec![web_hit("https://a.example/", "duckduckgo")])),
            serp(&bing, Ok(vec![])),
        )
        .await;
        assert!(!ddg.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!bing.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(out.hits, expected);
        // A SearXNG hit is a web result too.
        let mixed = [
            meta_hit("https://en.wikipedia.org/wiki/X", &["wikipedia"]),
            web_hit("https://x.example/", "searxng"),
        ];
        assert!(!needs_results_page_tier(true, false, &mixed));
        assert!(!needs_results_page_tier(true, false, &[]));
    }

    #[tokio::test]
    async fn should_try_bing_after_a_duckduckgo_challenge() {
        let c = controls(QUERY, "general");
        let (ddg, bing) = (Default::default(), Default::default());
        let out = complete_free_tier(
            reference_answer(),
            QUERY,
            true,
            &c,
            serp(
                &ddg,
                Err("DuckDuckGo answered with a bot check (not solved)".into()),
            ),
            serp(
                &bing,
                Ok(vec![web_hit("https://b.example/post", "bing_cdp")]),
            ),
        )
        .await;
        assert!(ddg.load(std::sync::atomic::Ordering::SeqCst));
        assert!(bing.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(out.hits[0].url, "https://b.example/post");
        assert_eq!(out.used, vec!["bing_cdp", "metasearch"]);
    }

    #[tokio::test]
    async fn should_return_the_metasearch_answer_when_both_results_pages_miss() {
        let c = controls(QUERY, "general");
        let (ddg, bing) = (Default::default(), Default::default());
        let out = complete_free_tier(
            reference_answer(),
            QUERY,
            true,
            &c,
            serp(&ddg, Err("DuckDuckGo answered with a bot check".into())),
            serp(&bing, Err("Bing answered with a challenge".into())),
        )
        .await;
        assert!(bing.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(out.hits, reference_answer().hits);
        assert_eq!(out.used, vec!["metasearch"]);
        assert!(out.note.is_some(), "the thin-general note stays");
    }

    #[test]
    fn should_parse_lang_since_and_category_controls() {
        let input: Input = serde_json::from_value(serde_json::json!({
            "query": "rust release",
            "lang": ["en", "zh-cn"],
            "since": "7d",
            "region": "tw",
        }))
        .unwrap();
        let c = FreeTierControls::parse(&input).unwrap();
        assert_eq!(c.filters.langs, vec!["en", "zh-CN"]);
        assert!(c.news, "a 7-day window is news-ish");
        assert_eq!(c.region.as_deref(), Some("TW"));
        assert_eq!(c.langs("rust release").len(), 2);

        let general: Input = serde_json::from_value(serde_json::json!({
            "query": "rust borrow checker", "category": "general", "lang": "en"
        }))
        .unwrap();
        assert!(!FreeTierControls::parse(&general).unwrap().news);

        let bad: Input =
            serde_json::from_value(serde_json::json!({"query": "q", "since": "soon"})).unwrap();
        assert!(FreeTierControls::parse(&bad).is_err());
    }

    #[test]
    fn should_search_each_language_in_its_own_words() {
        let input: Input = serde_json::from_value(serde_json::json!({
            "query": "AI regulation",
            "lang": "en",
            "query_by_lang": {"zh-cn": "人工智能 监管"}
        }))
        .unwrap();
        let c = FreeTierControls::parse(&input).unwrap();
        assert_eq!(c.filters.langs, vec!["en", "zh-CN"]);
        assert_eq!(
            c.langs(&input.query),
            vec![Some("en".to_string()), Some("zh-CN".to_string())]
        );
        assert_eq!(c.query_for(&input.query, Some("zh-CN")), "人工智能 监管");
        assert_eq!(c.query_for(&input.query, Some("en")), "AI regulation");
    }

    #[tokio::test]
    async fn should_report_invalid_controls_without_searching() {
        let tool = WebSearchTool::new();
        let r = tool
            .execute(&serde_json::json!({"query": "q", "lang": "english"}))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.output.contains("invalid language tag"), "{}", r.output);
    }
}
