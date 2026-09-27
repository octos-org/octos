//! Search providers, controls and the polite page reader for deep-search.
//!
//! Provider order (see `octos_research::plan`): free structured sources
//! first (GDELT + Google News RSS, for news-ish queries), then a
//! self-hosted SearXNG (`SEARXNG_URL`), then search APIs with keys, then the
//! keyless DuckDuckGo HTML endpoint. The headless-Chrome Bing scrape is not
//! part of the automatic order; it needs `OCTOS_ALLOW_BROWSER_SERP=1`.
//!
//! Pages that will be cited are read with an identifiable User-Agent,
//! after a robots.txt check, spaced per host, with size caps; JS-heavy pages
//! (no main text over plain HTTP) are rendered by the `deep_crawl` browser.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use octos_research::date::Since;
use octos_research::extract::{self, PageMeta};
use octos_research::plan::{self, Category, PlanInput, Provider};
use octos_research::providers as free;
use octos_research::{Filters, HostThrottle, OneOrMany, RobotsCache, SearchHit};

use crate::Input;

/// Largest page body read over plain HTTP.
const MAX_PAGE_BYTES: usize = 3 * 1024 * 1024;
/// Parallel page reads (per-host spacing still applies).
const READ_CONCURRENCY: usize = 8;

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// Parsed research controls.
pub(crate) struct Options {
    pub items_mode: bool,
    pub filters: Filters,
    pub region: Option<String>,
    pub category: Category,
    /// Render JS-heavy pages with the browser when plain HTTP has no text.
    pub render: bool,
    pub now: DateTime<Utc>,
}

impl Options {
    pub fn from_input(input: &Input, now: DateTime<Utc>) -> Result<Self, String> {
        let items_mode = match input.output.as_deref().map(str::trim) {
            None | Some("") | Some("report") => false,
            Some("items") => true,
            Some(other) => {
                return Err(format!(
                    "invalid output {other:?} (use \"report\" or \"items\")"
                ));
            }
        };
        let since = match input.since.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(s) => Some(Since::parse(s, now)?),
        };
        let filters = Filters::new(
            input.lang.clone().into_vec(),
            since,
            input.domains_allow.clone(),
            input.domains_deny.clone(),
            input.max_per_domain,
        )?;
        let region = input
            .region
            .as_deref()
            .map(|r| r.trim().to_ascii_uppercase())
            .filter(|r| !r.is_empty());
        if let Some(r) = &region {
            if r.len() != 2 || !r.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err(format!(
                    "invalid region {r:?} (use ISO 3166-1 alpha-2, e.g. US)"
                ));
            }
        }
        let render = match input.render.as_deref().map(str::trim) {
            None | Some("") | Some("auto") => true,
            Some("off") | Some("never") => false,
            Some(other) => return Err(format!("invalid render {other:?} (use auto or off)")),
        };
        Ok(Self {
            items_mode,
            filters,
            region,
            category: Category::parse(input.category.as_deref())?,
            render,
            now,
        })
    }

    /// Languages to search in: the requested ones, else a guess from the
    /// query script, else "provider default" (`None`).
    pub fn search_langs(&self, query: &str) -> Vec<Option<String>> {
        if self.filters.langs.is_empty() {
            vec![octos_research::lang::guess_from_script(query).map(String::from)]
        } else {
            self.filters.langs.iter().cloned().map(Some).collect()
        }
    }

    pub fn is_news(&self, query: &str) -> bool {
        self.category
            .is_news(query, self.filters.since.as_ref(), self.now)
    }

    pub fn controls_json(&self) -> serde_json::Value {
        let mut v = self.filters.to_json();
        v["region"] = serde_json::json!(self.region);
        v["category"] = serde_json::json!(self.category);
        v["render"] = serde_json::json!(if self.render { "auto" } else { "off" });
        v
    }
}

/// `lang` accepted as a string or a list.
pub(crate) type LangInput = OneOrMany;

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Result of one provider call.
#[derive(Default)]
pub(crate) struct ProviderOut {
    pub hits: Vec<SearchHit>,
    /// Model-written answer text (Perplexity, Tavily, Serper knowledge
    /// graph), used for the overview fallback and follow-up topics.
    pub answer: String,
}

/// Result of one search round across providers.
#[derive(Default)]
pub(crate) struct RoundOut {
    pub hits: Vec<SearchHit>,
    pub answer: String,
    pub providers: Vec<String>,
    pub errors: Vec<String>,
}

pub(crate) fn api_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(octos_research::USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

fn gdelt_throttle() -> &'static HostThrottle {
    static T: OnceLock<HostThrottle> = OnceLock::new();
    T.get_or_init(|| HostThrottle::new(free::GDELT_MIN_INTERVAL))
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Keyed providers with a key, in deep-search's historical priority.
fn keyed_available() -> Vec<Provider> {
    [
        (Provider::Serper, "SERPER_API_KEY"),
        (Provider::Tavily, "TAVILY_API_KEY"),
        (Provider::Perplexity, "PERPLEXITY_API_KEY"),
        (Provider::Brave, "BRAVE_API_KEY"),
        (Provider::You, "YDC_API_KEY"),
    ]
    .into_iter()
    .filter(|(_, k)| env_nonempty(k).is_some())
    .map(|(p, _)| p)
    .collect()
}

pub(crate) fn browser_serp_allowed() -> bool {
    octos_research::browser_serp_allowed(|k| std::env::var(k).ok())
}

/// Automatic provider plan for this environment.
pub(crate) fn auto_plan(news: bool) -> Vec<Provider> {
    plan::plan(&PlanInput {
        news,
        searxng_configured: env_nonempty(octos_research::SEARXNG_URL_ENV).is_some(),
        keyed: keyed_available(),
        keyless_fallback: true,
        allow_browser_serp: browser_serp_allowed(),
    })
}

/// One search round for `query` in `lang`.
///
/// `engine` = a provider id runs that provider first (falling back to the
/// automatic order if it yields nothing); `all` runs every planned provider
/// in parallel. Automatic order: the free tier and SearXNG run first; the
/// keyed/keyless tier (top two raced, as before) only runs when those did
/// not fill `count`.
pub(crate) async fn search_round(
    opts: &Options,
    engine: Option<&str>,
    query: &str,
    lang: Option<&str>,
    count: u8,
) -> RoundOut {
    let news = opts.is_news(query);
    let mut out = RoundOut::default();
    let plan = auto_plan(news);

    let explicit = engine
        .map(str::trim)
        .filter(|e| !e.is_empty() && *e != "auto");
    if let Some("all") = explicit {
        run_parallel(opts, &plan, query, lang, count, &mut out).await;
        return out;
    }
    if let Some(id) = explicit {
        match Provider::from_id(id) {
            Some(Provider::BingBrowser) if !browser_serp_allowed() => out.errors.push(format!(
                "bing_cdp: disabled (browser search-results scraping needs {}=1)",
                octos_research::BROWSER_SERP_ENV
            )),
            Some(p) => {
                run_parallel(opts, &[p], query, lang, count, &mut out).await;
                if !out.hits.is_empty() {
                    return out;
                }
            }
            None => out.errors.push(format!("unknown search_engine {id:?}")),
        }
    }

    // Tier 1+2: free structured sources and SearXNG.
    let first: Vec<Provider> = plan
        .iter()
        .copied()
        .filter(|p| p.is_free_structured() || *p == Provider::Searxng)
        .collect();
    if !first.is_empty() {
        run_parallel(opts, &first, query, lang, count, &mut out).await;
    }
    if out.hits.len() >= count as usize {
        return out;
    }

    // Tier 3: keyed APIs + keyless fallback, top two raced (historical
    // deep-search behaviour, minus the browser scrape).
    let rest: Vec<Provider> = plan
        .iter()
        .copied()
        .filter(|p| !p.is_free_structured() && *p != Provider::Searxng)
        .take(2)
        .collect();
    if !rest.is_empty() {
        run_parallel(opts, &rest, query, lang, count, &mut out).await;
    }
    out
}

async fn run_parallel(
    opts: &Options,
    providers: &[Provider],
    query: &str,
    lang: Option<&str>,
    count: u8,
    out: &mut RoundOut,
) {
    let futs = providers.iter().map(|p| async move {
        let r = tokio::time::timeout(
            Duration::from_secs(40),
            run_provider(opts, *p, query, lang, count),
        )
        .await
        .unwrap_or_else(|_| Err("timed out".to_string()));
        (*p, r)
    });
    let results = futures::future::join_all(futs).await;
    let mut seen: HashSet<String> = out
        .hits
        .iter()
        .map(|h| octos_research::urls::dedup_key(&h.url))
        .collect();
    for (p, r) in results {
        match r {
            Ok(po) => {
                if po.hits.is_empty() && po.answer.is_empty() {
                    out.errors.push(format!("{}: no results", p.id()));
                    continue;
                }
                out.providers.push(p.id().to_string());
                if !po.answer.trim().is_empty() {
                    if !out.answer.is_empty() {
                        out.answer.push_str("\n\n");
                    }
                    out.answer.push_str(po.answer.trim());
                }
                for h in po.hits {
                    if seen.insert(octos_research::urls::dedup_key(&h.url)) {
                        out.hits.push(h);
                    }
                }
            }
            Err(e) => out.errors.push(format!("{}: {e}", p.id())),
        }
    }
}

async fn get_text(req: reqwest::RequestBuilder, label: &str) -> Result<String, String> {
    let resp = req
        .send()
        .await
        .map_err(|e| format!("{label} error: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let head: String = text.chars().take(200).collect();
        return Err(format!("{label} HTTP {status}: {head}"));
    }
    Ok(text)
}

async fn run_provider(
    opts: &Options,
    p: Provider,
    query: &str,
    lang: Option<&str>,
    count: u8,
) -> Result<ProviderOut, String> {
    let client = api_client();
    let since = opts.filters.since.as_ref();
    let bucket = since.map(|s| s.bucket(opts.now));
    let news = opts.is_news(query);
    let lang_primary = lang.map(octos_research::lang::primary);
    let key = |k: &str| env_nonempty(k).ok_or_else(|| format!("{k} not set"));
    let hits = match p {
        Provider::Gdelt => {
            gdelt_throttle().wait("api.gdeltproject.org", None).await;
            let url = free::gdelt_request_url(query, lang, since, count as usize, opts.now);
            let body = get_text(client.get(url), "GDELT").await?;
            ProviderOut {
                hits: free::parse_gdelt(&body)?,
                answer: String::new(),
            }
        }
        Provider::GoogleNewsRss => {
            let url = free::google_news_rss_url(query, lang, opts.region.as_deref(), since);
            let body = get_text(client.get(url), "Google News RSS").await?;
            let default_lang = free::google_news_lang(lang, opts.region.as_deref());
            let mut hits = free::parse_feed(&body, "google_news_rss", Some(&default_lang))?;
            hits.truncate(count as usize);
            ProviderOut {
                hits,
                answer: String::new(),
            }
        }
        Provider::Searxng => {
            let base = key(octos_research::SEARXNG_URL_ENV)?;
            let url = free::searxng_request_url(&base, query, lang, since, news, opts.now)?;
            let body = get_text(client.get(url), "SearXNG").await?;
            let mut hits = free::parse_searxng(&body)?;
            hits.truncate(count as usize);
            ProviderOut {
                hits,
                answer: String::new(),
            }
        }
        Provider::Serper => {
            let mut body = serde_json::json!({"q": query, "num": count.min(10)});
            if let Some(l) = &lang_primary {
                body["hl"] = serde_json::json!(l);
            }
            if let Some(r) = &opts.region {
                body["gl"] = serde_json::json!(r.to_ascii_lowercase());
            }
            if let Some(b) = bucket {
                body["tbs"] = serde_json::json!(b.qdr());
            }
            let text = get_text(
                client
                    .post("https://google.serper.dev/search")
                    .header("X-API-KEY", key("SERPER_API_KEY")?)
                    .json(&body),
                "Serper",
            )
            .await?;
            parse_serper(&text)?
        }
        Provider::Tavily => {
            let mut body = serde_json::json!({
                "api_key": key("TAVILY_API_KEY")?,
                "query": query,
                "max_results": count.min(10),
                "include_answer": true,
                "include_raw_content": false,
            });
            if news {
                body["topic"] = serde_json::json!("news");
            }
            if let Some(b) = bucket {
                body["time_range"] = serde_json::json!(b.as_word());
            }
            let text = get_text(
                client.post("https://api.tavily.com/search").json(&body),
                "Tavily",
            )
            .await?;
            parse_tavily(&text)?
        }
        Provider::Perplexity => {
            let mut body = serde_json::json!({
                "model": "sonar",
                "messages": [{"role": "user", "content": query}],
                "max_tokens": 1024
            });
            if let Some(b) = bucket {
                body["search_recency_filter"] = serde_json::json!(b.as_word());
            }
            let text = get_text(
                client
                    .post("https://api.perplexity.ai/chat/completions")
                    .bearer_auth(key("PERPLEXITY_API_KEY")?)
                    .json(&body),
                "Perplexity",
            )
            .await?;
            parse_perplexity(&text)?
        }
        Provider::Brave => {
            let mut params = vec![
                ("q".to_string(), query.to_string()),
                ("count".to_string(), count.to_string()),
            ];
            if let Some(l) = &lang_primary {
                params.push(("search_lang".into(), l.clone()));
            }
            if let Some(r) = &opts.region {
                params.push(("country".into(), r.clone()));
            }
            if let Some(b) = bucket {
                params.push(("freshness".into(), b.brave().to_string()));
            }
            let text = get_text(
                client
                    .get("https://api.search.brave.com/res/v1/web/search")
                    .header("X-Subscription-Token", key("BRAVE_API_KEY")?)
                    .header("Accept", "application/json")
                    .query(&params),
                "Brave",
            )
            .await?;
            parse_brave(&text)?
        }
        Provider::You => {
            let text = get_text(
                client
                    .get("https://ydc-index.io/v1/search")
                    .header("X-API-Key", key("YDC_API_KEY")?)
                    .query(&[("query", query), ("count", &count.to_string())]),
                "You.com",
            )
            .await?;
            parse_you(&text)?
        }
        Provider::DuckDuckGo => ProviderOut {
            hits: crate::ddg_search(query, count).await?,
            answer: String::new(),
        },
        Provider::BingBrowser => {
            if !browser_serp_allowed() {
                return Err("disabled".to_string());
            }
            ProviderOut {
                hits: crate::bing_cdp_search(query, count).await?,
                answer: String::new(),
            }
        }
        Provider::Exa => return Err("not supported by deep-search".to_string()),
    };
    Ok(hits)
}

fn hit(url: &str, title: &str, snippet: &str, provider: &str) -> Option<SearchHit> {
    let url = url.trim();
    if !url.starts_with("http") {
        return None;
    }
    Some(SearchHit {
        url: url.to_string(),
        title: title.trim().to_string(),
        snippet: snippet.trim().to_string(),
        provider: provider.to_string(),
        ..Default::default()
    })
}

fn json(text: &str, label: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(text).map_err(|e| format!("{label} parse error: {e}"))
}

fn s<'a>(v: &'a serde_json::Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

pub(crate) fn parse_serper(text: &str) -> Result<ProviderOut, String> {
    let data = json(text, "Serper")?;
    let mut answer = String::new();
    if let Some(kg) = data.get("knowledgeGraph") {
        if !s(kg, "title").is_empty() {
            answer = format!("**{}**: {}", s(kg, "title"), s(kg, "description"));
        }
    }
    let hits = data["organic"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let mut h = hit(s(r, "link"), s(r, "title"), s(r, "snippet"), "serper")?;
            h.published = octos_research::date::to_iso(s(r, "date"));
            Some(h)
        })
        .collect();
    Ok(ProviderOut { hits, answer })
}

pub(crate) fn parse_tavily(text: &str) -> Result<ProviderOut, String> {
    let data = json(text, "Tavily")?;
    let answer = match s(&data, "answer") {
        "" => String::new(),
        a => format!("**AI Summary:**\n{a}"),
    };
    let hits = data["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let mut h = hit(s(r, "url"), s(r, "title"), s(r, "content"), "tavily")?;
            h.published = octos_research::date::to_iso(s(r, "published_date"));
            Some(h)
        })
        .collect();
    Ok(ProviderOut { hits, answer })
}

pub(crate) fn parse_brave(text: &str) -> Result<ProviderOut, String> {
    let data = json(text, "Brave")?;
    let hits = data["web"]["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let mut h = hit(s(r, "url"), s(r, "title"), s(r, "description"), "brave")?;
            h.published = octos_research::date::to_iso(s(r, "page_age"));
            h.lang = octos_research::lang::normalize(s(r, "language"));
            Some(h)
        })
        .collect();
    Ok(ProviderOut {
        hits,
        answer: String::new(),
    })
}

pub(crate) fn parse_you(text: &str) -> Result<ProviderOut, String> {
    let data = json(text, "You.com")?;
    let hits = data["results"]["web"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let snippet = match s(r, "description") {
                "" => r["snippets"][0].as_str().unwrap_or(""),
                d => d,
            };
            hit(s(r, "url"), s(r, "title"), snippet, "you")
        })
        .collect();
    Ok(ProviderOut {
        hits,
        answer: String::new(),
    })
}

pub(crate) fn parse_perplexity(text: &str) -> Result<ProviderOut, String> {
    let data = json(text, "Perplexity")?;
    let answer = data["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let mut hits: Vec<SearchHit> = data["search_results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let mut h = hit(s(r, "url"), s(r, "title"), "", "perplexity")?;
            h.published = octos_research::date::to_iso(s(r, "date"));
            Some(h)
        })
        .collect();
    if hits.is_empty() {
        hits = data["citations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| hit(c.as_str()?, "", "", "perplexity"))
            .collect();
    }
    Ok(ProviderOut { hits, answer })
}

// ---------------------------------------------------------------------------
// Reading pages
// ---------------------------------------------------------------------------

/// A page read for citation.
pub(crate) struct ReadPage {
    pub final_url: String,
    pub text: String,
    pub meta: PageMeta,
    pub links: Vec<String>,
    pub rendered: bool,
    pub fetched_at: String,
}

impl ReadPage {
    /// Canonical URL for the item: the page's own canonical link, else the
    /// final URL, both without tracking parameters.
    pub fn canonical_url(&self) -> String {
        octos_research::urls::canonicalize(
            self.meta.canonical.as_deref().unwrap_or(&self.final_url),
        )
    }
}

/// Polite reader: robots.txt per origin, per-host spacing, size caps,
/// identifiable User-Agent, browser rendering for JS-heavy pages.
pub(crate) struct Reader {
    robots: RobotsCache,
    throttle: HostThrottle,
    render: bool,
}

impl Reader {
    pub fn new(render: bool) -> Self {
        let interval_ms = std::env::var("DEEP_SEARCH_HOST_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1000)
            .clamp(200, 30_000);
        Self {
            robots: RobotsCache::new(),
            throttle: HostThrottle::new(Duration::from_millis(interval_ms)),
            render,
        }
    }

    async fn robots_allows(&self, url: &str) -> Result<Option<Duration>, String> {
        let d = self
            .robots
            .check(url, octos_research::AGENT_TOKEN, |robots_url| async move {
                match crate::ssrf_safe_get(&robots_url).await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let body = read_capped(resp, 512 * 1024).await.unwrap_or_default();
                        (Some(status), body)
                    }
                    Err(_) => (None, String::new()),
                }
            })
            .await;
        if d.allowed {
            Ok(d.crawl_delay)
        } else {
            Err(d.reason.to_string())
        }
    }

    /// Read one page. `Err(reason)` is recorded as a skipped URL.
    pub async fn read(&self, url: &str) -> Result<ReadPage, String> {
        if crate::is_private_url(url) {
            return Err("blocked_private_host".to_string());
        }
        let crawl_delay = self.robots_allows(url).await?;
        let host = octos_research::urls::domain_of(url).unwrap_or_default();
        self.throttle.wait(&host, crawl_delay).await;

        let resp = crate::ssrf_safe_get(url)
            .await
            .map_err(|e| format!("fetch_error: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("fetch_error: HTTP {}", status.as_u16()));
        }
        let final_url = resp.url().to_string();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !ctype.is_empty()
            && !ctype.contains("html")
            && !ctype.contains("xml")
            && !ctype.starts_with("text/")
        {
            return Err(format!("unsupported_content_type: {ctype}"));
        }
        let body = read_capped(resp, MAX_PAGE_BYTES)
            .await
            .map_err(|e| format!("fetch_error: {e}"))?;

        let fetched_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        if ctype.starts_with("text/plain") {
            return Ok(ReadPage {
                final_url,
                text: body.trim().to_string(),
                meta: PageMeta::default(),
                links: Vec::new(),
                rendered: false,
                fetched_at,
            });
        }

        let mut ex = extract::extract(&body, &final_url);
        let mut html = body;
        let mut page_url = final_url;
        let mut rendered = false;
        if ex.is_empty_text() {
            // Readability found no article; keep the plain boilerplate-
            // stripped text when it is substantial (lists, docs pages).
            let plain = crate::html_to_text(&html);
            if plain.chars().filter(|c| !c.is_whitespace()).count() >= extract::MIN_MAIN_TEXT_CHARS
            {
                ex.text = plain;
            }
        }
        if ex.is_empty_text() && self.render {
            match render_with_browser(&page_url).await {
                Ok((rurl, rhtml)) => {
                    // The browser may land somewhere else (JS redirect):
                    // that origin's robots.txt applies too.
                    if octos_research::urls::domain_of(&rurl)
                        != octos_research::urls::domain_of(&page_url)
                    {
                        self.robots_allows(&rurl).await?;
                    }
                    let rex = extract::extract(&rhtml, &rurl);
                    if !rex.is_empty_text() {
                        ex = rex;
                        html = rhtml;
                        page_url = rurl;
                        rendered = true;
                    }
                }
                Err(e) => eprintln!("[deep_search] render skipped for {page_url}: {e}"),
            }
        }
        if ex.is_empty_text() {
            return Err("no_main_text".to_string());
        }
        let links = crate::extract_links_from_html(&html, &page_url);
        Ok(ReadPage {
            final_url: page_url,
            text: ex.text,
            meta: ex.meta,
            links,
            rendered,
            fetched_at,
        })
    }

    /// Split hits into those robots.txt lets us read and those it does not
    /// (with the reason), before they take a slot in the page budget.
    /// Checks run concurrently; each origin's robots.txt is fetched once.
    pub async fn robots_partition(
        &self,
        hits: Vec<SearchHit>,
    ) -> (Vec<SearchHit>, Vec<(SearchHit, String)>) {
        let decisions: Vec<Result<Option<Duration>, String>> = stream::iter(hits.iter())
            .map(|h| self.robots_allows(&h.url))
            .buffered(READ_CONCURRENCY)
            .collect()
            .await;
        let mut ok = Vec::new();
        let mut denied = Vec::new();
        for (h, d) in hits.into_iter().zip(decisions) {
            match d {
                Ok(_) => ok.push(h),
                Err(reason) => denied.push((h, reason)),
            }
        }
        (ok, denied)
    }

    /// Read many URLs concurrently, preserving input order.
    pub async fn read_all(&self, urls: &[String]) -> Vec<Result<ReadPage, String>> {
        stream::iter(urls.iter())
            .map(|u| self.read(u))
            .buffered(READ_CONCURRENCY)
            .collect()
            .await
    }
}

/// Read a response body up to `cap` bytes (lossy UTF-8).
async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<String, String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = cap.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if buf.len() >= cap {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("read body failed: {e}")),
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Render `url` in the `deep_crawl` headless browser and return
/// `(final_url, html)`. Reading only: one page, no search-results pages.
async fn render_with_browser(url: &str) -> Result<(String, String), String> {
    let _permit = crate::browser_semaphore()
        .acquire()
        .await
        .map_err(|_| "browser semaphore closed".to_string())?;
    let bin = crate::find_deep_crawl_bin().ok_or("deep_crawl binary not found")?;
    let input = serde_json::json!({
        "url": url,
        "max_depth": 0,
        "max_pages": 1,
        "include_html": true,
    });
    let stdout = crate::run_deep_crawl(&bin, &input, Duration::from_secs(45)).await?;
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|_| "unparseable deep_crawl output".to_string())?;
    let page = parsed["pages"]
        .as_array()
        .and_then(|p| p.first())
        .ok_or("browser returned no page")?;
    let html = page["html"].as_str().unwrap_or("").to_string();
    if html.is_empty() {
        return Err("browser returned empty HTML".to_string());
    }
    let final_url = page["final_url"].as_str().unwrap_or(url).to_string();
    Ok((final_url, html))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn input(v: serde_json::Value) -> Input {
        serde_json::from_value(v).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap()
    }

    #[test]
    fn should_parse_controls_from_input() {
        let o = Options::from_input(
            &input(serde_json::json!({
                "query": "q",
                "output": "items",
                "lang": ["en", "zh-cn"],
                "region": "us",
                "since": "7d",
                "max_per_domain": 2,
                "domains_deny": ["example.com"],
                "category": "news",
                "render": "off"
            })),
            now(),
        )
        .unwrap();
        assert!(o.items_mode);
        assert_eq!(o.filters.langs, vec!["en", "zh-CN"]);
        assert_eq!(o.region.as_deref(), Some("US"));
        assert_eq!(o.filters.max_per_domain, Some(2));
        assert!(o.filters.since.is_some());
        assert_eq!(o.category, Category::News);
        assert!(!o.render);
        assert_eq!(
            o.search_langs("q"),
            vec![Some("en".to_string()), Some("zh-CN".to_string())]
        );
    }

    #[test]
    fn should_default_to_report_mode_and_guess_lang_from_script() {
        let o = Options::from_input(
            &input(serde_json::json!({"query": "q", "lang": "ja"})),
            now(),
        )
        .unwrap();
        assert!(!o.items_mode);
        assert!(o.render);
        let o = Options::from_input(&input(serde_json::json!({"query": "q"})), now()).unwrap();
        assert_eq!(o.search_langs("台风最新消息"), vec![Some("zh".to_string())]);
        assert_eq!(o.search_langs("typhoon"), vec![None]);
    }

    #[test]
    fn should_reject_invalid_controls() {
        for bad in [
            serde_json::json!({"query": "q", "output": "xml"}),
            serde_json::json!({"query": "q", "since": "soon"}),
            serde_json::json!({"query": "q", "lang": "english"}),
            serde_json::json!({"query": "q", "region": "USA"}),
            serde_json::json!({"query": "q", "category": "sports"}),
        ] {
            assert!(
                Options::from_input(&input(bad.clone()), now()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn should_parse_keyed_provider_responses_into_hits() {
        let serper = r#"{"knowledgeGraph":{"title":"Rust","description":"A language"},
            "organic":[{"title":"Rust","link":"https://rust-lang.org","snippet":"Fast"}]}"#;
        let o = parse_serper(serper).unwrap();
        assert_eq!(o.hits[0].url, "https://rust-lang.org");
        assert!(o.answer.contains("**Rust**"));

        let tavily = r#"{"answer":"A.","results":[{"title":"T","url":"https://t.example/a",
            "content":"C","published_date":"2026-09-25"}]}"#;
        let o = parse_tavily(tavily).unwrap();
        assert_eq!(o.hits[0].published.as_deref(), Some("2026-09-25"));

        let brave = r#"{"web":{"results":[{"title":"B","url":"https://b.example/",
            "description":"D","language":"de","page_age":"2026-09-24T10:00:00"}]}}"#;
        let o = parse_brave(brave).unwrap();
        assert_eq!(o.hits[0].lang.as_deref(), Some("de"));

        let pplx = r#"{"choices":[{"message":{"content":"Answer [1]"}}],
            "citations":["https://p.example/1"]}"#;
        let o = parse_perplexity(pplx).unwrap();
        assert_eq!(o.hits[0].url, "https://p.example/1");
        assert_eq!(o.answer, "Answer [1]");
    }

    #[test]
    fn should_not_plan_browser_serp_without_opt_in() {
        // The env var is not set in the test environment.
        if std::env::var(octos_research::BROWSER_SERP_ENV).is_ok() {
            return;
        }
        for news in [true, false] {
            assert!(
                !auto_plan(news).contains(&Provider::BingBrowser),
                "bing_cdp must not be in the automatic order"
            );
        }
        assert_eq!(auto_plan(true)[0], Provider::Gdelt);
    }

    #[tokio::test]
    async fn should_refuse_explicit_bing_without_opt_in() {
        if std::env::var(octos_research::BROWSER_SERP_ENV).is_ok() {
            return;
        }
        let o = Options::from_input(&input(serde_json::json!({"query": "q"})), now()).unwrap();
        let r = run_provider(&o, Provider::BingBrowser, "q", None, 3).await;
        assert_eq!(r.err().as_deref(), Some("disabled"));
    }
}
