//! Search providers, controls and the polite page reader for deep-search.
//!
//! Provider order (see `octos_research::plan`): the octos metasearch first
//! (key-less OctoScript engines over official APIs and feeds: GDELT, Hacker
//! News, Wikipedia, arXiv, ...; disable with `OCTOS_METASEARCH=0` to call
//! GDELT + Google News RSS directly for news), then a
//! self-hosted SearXNG (`SEARXNG_URL`), then search APIs with keys, then
//! results-page search (DuckDuckGo HTML, Bing in headless Chrome) for general
//! web results, on unless `OCTOS_ALLOW_SERP_SCRAPE=0` (alias
//! `OCTOS_ALLOW_BROWSER_SERP`). If nothing returns anything, the result is
//! empty and says how to add SearXNG or a key.
//!
//! Pages that will be cited are read with an identifiable User-Agent,
//! after a robots.txt check, spaced per host, with size caps; JS-heavy pages
//! (no main text over plain HTTP) are rendered by the `deep_crawl` browser.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use octos_research::date::Since;
use octos_research::extract::PageMeta;
use octos_research::plan::{self, Category, PlanInput, Provider};
use octos_research::providers as free;
use octos_research::reader;
use octos_research::{Filters, HostThrottle, OneOrMany, SearchHit};

use crate::Input;

/// Parallel page reads (per-host spacing still applies).
const READ_CONCURRENCY: usize = 8;

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// Parsed research controls.
pub(crate) struct Options {
    pub items_mode: bool,
    /// Per-language queries (normalized tag → query).
    pub query_by_lang: std::collections::BTreeMap<String, String>,
    pub filters: Filters,
    pub region: Option<String>,
    pub category: Category,
    /// Render JS-heavy pages with the browser when plain HTTP has no text.
    pub render: bool,
    /// Results-page search on (env; on unless turned off).
    pub allow_serp_scrape: bool,
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
        let query_by_lang = octos_research::lang::parse_query_by_lang(&input.query_by_lang)?;
        let mut langs = input.lang.clone().into_vec();
        // Languages with their own query are searched (and, when the caller
        // restricted languages, kept) too.
        if !langs.is_empty() {
            langs.extend(query_by_lang.keys().cloned());
        }
        let filters = Filters::new(
            langs,
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
            query_by_lang,
            filters,
            region,
            category: Category::parse(input.category.as_deref())?,
            render,
            allow_serp_scrape: serp_scrape_allowed(),
            now,
        })
    }

    /// Languages to search in: the requested ones, else a guess from the
    /// query script, else "provider default" (`None`).
    pub fn search_langs(&self, query: &str) -> Vec<Option<String>> {
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

    /// The query for one language round.
    pub fn query_for<'a>(&'a self, query: &'a str, lang: Option<&str>) -> &'a str {
        octos_research::lang::query_for(query, &self.query_by_lang, lang)
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
        v["respect_robots"] =
            serde_json::json!(octos_research::respect_robots(|k| std::env::var(k).ok()));
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
    /// Provider notes worth passing on (e.g. the metasearch saying key-less
    /// general search is limited).
    pub notes: Vec<String>,
}

/// Result of one search round across providers.
#[derive(Default)]
pub(crate) struct RoundOut {
    pub hits: Vec<SearchHit>,
    pub answer: String,
    pub providers: Vec<String>,
    pub errors: Vec<String>,
    /// Every provider called this round (with or without results).
    pub tried: Vec<String>,
    pub notes: Vec<String>,
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

/// Whether results-page search (DuckDuckGo HTML, Bing in headless Chrome)
/// is on: yes unless `OCTOS_ALLOW_SERP_SCRAPE=0`.
pub(crate) fn serp_scrape_allowed() -> bool {
    octos_research::serp_scrape_allowed(|k| std::env::var(k).ok())
}

/// Upgrade visibility: when a results-page provider runs only because of
/// the new default, say so once on stderr (the skill's log).
fn default_notice_once() {
    static NOTICE: std::sync::Once = std::sync::Once::new();
    if let Some(notice) = octos_research::serp_scrape_default_notice(|k| std::env::var(k).ok()) {
        NOTICE.call_once(|| eprintln!("[deep-search] {notice}"));
    }
}

/// Automatic provider plan for this environment.
pub(crate) fn auto_plan(news: bool, allow_serp_scrape: bool) -> Vec<Provider> {
    plan::plan(&PlanInput {
        news,
        metasearch: octos_research::metasearch::enabled(|k| std::env::var(k).ok()),
        searxng_configured: env_nonempty(octos_research::SEARXNG_URL_ENV).is_some(),
        keyed: keyed_available(),
        allow_serp_scrape,
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
    let plan = auto_plan(news, opts.allow_serp_scrape);

    let explicit = engine
        .map(str::trim)
        .filter(|e| !e.is_empty() && *e != "auto");
    if let Some("all") = explicit {
        run_parallel(opts, &plan, query, lang, count, &mut out).await;
        return out;
    }
    if let Some(id) = explicit {
        match Provider::from_id(id) {
            Some(p) if p.is_serp_scrape() && !opts.allow_serp_scrape => out.errors.push(format!(
                "{}: disabled (scraping search-results pages needs {}=1)",
                p.id(),
                octos_research::SERP_SCRAPE_ENV
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

    // Tier 3: keyed APIs (plus the opted-in scrapers), top two raced
    // (historical deep-search behaviour).
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
    for p in providers {
        if !out.tried.iter().any(|t| t == p.id()) {
            out.tried.push(p.id().to_string());
        }
    }
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
                for n in po.notes {
                    if !out.notes.contains(&n) {
                        out.notes.push(n);
                    }
                }
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
        Provider::Metasearch => metasearch_round(opts, query, lang, count).await?,
        Provider::Gdelt => {
            gdelt_throttle().wait("api.gdeltproject.org", None).await;
            let url = free::gdelt_request_url(query, lang, since, count as usize, opts.now);
            let body = get_text(client.get(url), "GDELT").await?;
            ProviderOut {
                hits: free::parse_gdelt(&body)?,
                answer: String::new(),
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                "messages": [{"role": "user", "content": query}]
                // No max_tokens: output size is left to the provider (standing
                // decision: no hard-coded output caps on model calls).
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
        Provider::DuckDuckGo => {
            if !opts.allow_serp_scrape {
                return Err("disabled".to_string());
            }
            default_notice_once();
            ProviderOut {
                hits: crate::ddg_search(query, count).await?,
                answer: String::new(),
                ..Default::default()
            }
        }
        Provider::BingBrowser => {
            if !opts.allow_serp_scrape {
                return Err("disabled".to_string());
            }
            default_notice_once();
            ProviderOut {
                hits: crate::bing_cdp_search(query, count).await?,
                answer: String::new(),
                ..Default::default()
            }
        }
        Provider::Exa => return Err("not supported by deep-search".to_string()),
    };
    Ok(hits)
}

/// The process-wide metasearch (shares rate limits, cache and engine
/// health across rounds).
fn metasearch() -> &'static octos_research::metasearch::Metasearch {
    static M: OnceLock<octos_research::metasearch::Metasearch> = OnceLock::new();
    M.get_or_init(|| {
        // Engines that render (Google) load in the person's browser.
        octos_research::metasearch::Metasearch::from_env(
            octos_research::metasearch::default_fetch(),
            &Default::default(),
        )
    })
}

/// One metasearch call for `query` in `lang`.
async fn metasearch_round(
    opts: &Options,
    query: &str,
    lang: Option<&str>,
    count: u8,
) -> Result<ProviderOut, String> {
    let since = opts.filters.since.as_ref();
    let mut req = octos_research::metasearch::SearchRequest::new(
        query,
        opts.category.metasearch_category(query, since, opts.now),
    );
    req.langs = lang.map(|l| vec![l.to_string()]).unwrap_or_default();
    req.region = opts.region.clone();
    req.since = opts.filters.since.clone();
    req.count = count as usize;
    req.limit = count as usize * 2;
    req.filters = opts.filters.clone();
    req.now = opts.now;
    req.results_pages = opts.allow_serp_scrape;
    let resp = metasearch().search(&req).await;
    if resp.items.is_empty() {
        let engines: Vec<String> = resp
            .engines
            .iter()
            .map(|e| match &e.error {
                Some(err) => format!("{} {:?}: {err}", e.engine, e.status),
                None => format!("{} {:?}", e.engine, e.status),
            })
            .collect();
        let mut msg = format!("no results ({})", engines.join("; "));
        if let Some(note) = resp.note {
            msg.push_str(&format!(". {note}"));
        }
        return Err(msg);
    }
    Ok(ProviderOut {
        hits: resp.hits(),
        answer: String::new(),
        notes: resp
            .note
            .iter()
            .cloned()
            .chain(resp.challenges())
            .chain(resp.browser_notice().map(String::from))
            .collect(),
    })
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
    Ok(ProviderOut {
        hits,
        answer,
        ..Default::default()
    })
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
    Ok(ProviderOut {
        hits,
        answer,
        ..Default::default()
    })
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
        ..Default::default()
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
        ..Default::default()
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
    Ok(ProviderOut {
        hits,
        answer,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Reading pages
// ---------------------------------------------------------------------------

/// A page read for citation (the shared reader's page plus its links).
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

    fn from_shared(p: reader::ReadPage) -> Self {
        let links = if p.html.is_empty() {
            Vec::new()
        } else {
            crate::extract_links_from_html(&p.html, &p.final_url)
        };
        Self {
            final_url: p.final_url,
            text: p.text,
            meta: p.meta,
            links,
            rendered: p.rendered,
            fetched_at: p.fetched_at,
        }
    }
}

/// deep-search's use of the shared polite reader (`octos_research::reader`:
/// SSRF + DNS pinning, robots.txt, per-host spacing, size caps, and
/// post-render SSRF re-validation), with the `deep_crawl` browser as the
/// renderer.
pub(crate) struct Reader {
    inner: reader::Reader,
}

impl Reader {
    pub fn new(render: bool) -> Self {
        let interval_ms = std::env::var("DEEP_SEARCH_HOST_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1000)
            .clamp(200, 30_000);
        let renderer: Option<reader::Renderer> = render.then(|| {
            Arc::new(|url: String| {
                Box::pin(async move { render_with_browser(&url).await }) as reader::RenderFuture
            }) as reader::Renderer
        });
        Self {
            inner: reader::Reader::new(reader::ReaderConfig {
                host_interval: Duration::from_millis(interval_ms),
                keep_html: true,
                respect_robots: octos_research::respect_robots(|k| std::env::var(k).ok()),
                fallback_text: Some(crate::html_to_text),
                renderer,
                ..Default::default()
            }),
        }
    }

    /// Read one page. A failure is recorded as a skipped URL with its
    /// reason and final URL.
    pub async fn read(&self, url: &str) -> Result<ReadPage, octos_research::ReadError> {
        self.inner.read(url).await.map(ReadPage::from_shared)
    }

    /// Split hits into those robots.txt lets us read and those it does not
    /// (with the reason), before they take a slot in the page budget.
    /// Checks run concurrently; each origin's robots.txt is fetched once.
    pub async fn robots_partition(
        &self,
        hits: Vec<SearchHit>,
    ) -> (Vec<SearchHit>, Vec<(SearchHit, String)>) {
        let decisions: Vec<Result<Option<Duration>, String>> = stream::iter(hits.iter())
            .map(|h| self.inner.robots_allows(&h.url))
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
    pub async fn read_all(
        &self,
        urls: &[String],
    ) -> Vec<Result<ReadPage, octos_research::ReadError>> {
        stream::iter(urls.iter())
            .map(|u| self.read(u))
            .buffered(READ_CONCURRENCY)
            .collect()
            .await
    }
}

/// Render `url` in the `deep_crawl` headless browser. Reading only: one
/// page, no search-results pages. deep_crawl blocks private destinations
/// inside the browser (request interception) and reports the navigation
/// chain; the shared reader re-validates it before accepting the HTML.
async fn render_with_browser(url: &str) -> Result<reader::Rendered, String> {
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
    parse_render_output(&stdout, url)
}

/// Parse deep_crawl's `pages[0]` into a [`reader::Rendered`].
fn parse_render_output(stdout: &str, url: &str) -> Result<reader::Rendered, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(stdout).map_err(|_| "unparseable deep_crawl output".to_string())?;
    let page = parsed["pages"]
        .as_array()
        .and_then(|p| p.first())
        .ok_or("browser returned no page")?;
    // deep_crawl's own verdict (e.g. "blocked by a bot challenge (not
    // bypassed)"); the shared reader classifies the message.
    if let Some(error) = page["error"].as_str().filter(|e| !e.is_empty()) {
        return Err(error.to_string());
    }
    let html = page["html"].as_str().unwrap_or("").to_string();
    if html.is_empty() {
        return Err("browser returned empty HTML".to_string());
    }
    Ok(reader::Rendered {
        final_url: page["final_url"].as_str().unwrap_or(url).to_string(),
        html,
        navigations: page["navigations"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        status: None,
    })
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
    fn should_search_each_language_in_its_own_words() {
        let o = Options::from_input(
            &input(serde_json::json!({
                "query": "AI regulation",
                "query_by_lang": {"zh-cn": "人工智能 监管"}
            })),
            now(),
        )
        .unwrap();
        assert!(o.filters.langs.is_empty(), "no lang filter was asked for");
        let langs = o.search_langs("AI regulation");
        assert!(langs.contains(&Some("zh-CN".to_string())), "{langs:?}");
        assert_eq!(o.query_for("AI regulation", Some("zh-CN")), "人工智能 监管");
        assert_eq!(o.query_for("AI regulation", Some("zh")), "人工智能 监管");
        assert_eq!(o.query_for("AI regulation", None), "AI regulation");

        let o = Options::from_input(
            &input(serde_json::json!({
                "query": "AI regulation",
                "lang": "en",
                "query_by_lang": {"zh": "人工智能 监管"}
            })),
            now(),
        )
        .unwrap();
        assert_eq!(o.filters.langs, vec!["en", "zh"], "kept, not filtered out");

        for bad in [
            serde_json::json!({"query": "q", "query_by_lang": {"chinese!": "x"}}),
            serde_json::json!({"query": "q", "query_by_lang": {"zh": "  "}}),
        ] {
            assert!(Options::from_input(&input(bad), now()).is_err());
        }
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

    #[tokio::test]
    async fn should_discard_browser_render_that_js_redirects_to_a_private_ip() {
        // deep_crawl output for the fixture page that JS-redirects to the
        // cloud metadata endpoint.
        let stdout = serde_json::json!({
            "output": "", "success": true,
            "pages": [{
                "url": "http://93.184.216.34/start",
                "final_url": "http://169.254.169.254/latest/meta-data/",
                "navigations": ["http://93.184.216.34/start", "http://169.254.169.254/latest/meta-data/"],
                "html": format!("<html><body><p>{}</p></body></html>", "secret ".repeat(80)),
            }]
        })
        .to_string();
        let rendered = parse_render_output(&stdout, "http://93.184.216.34/start").unwrap();
        assert_eq!(rendered.navigations.len(), 2);
        let r = reader::Reader::new(reader::ReaderConfig::default());
        let err = r
            .accept_rendered("http://93.184.216.34/start", rendered)
            .await
            .unwrap_err();
        assert_eq!(err.reason, octos_research::ReadFailure::Blocked, "{err}");
        assert!(err.to_string().contains("169.254.169.254"), "{err}");
    }

    #[test]
    fn should_report_deep_crawl_page_error_when_browser_hit_a_challenge() {
        let stdout = serde_json::json!({
            "output": "",
            "success": true,
            "pages": [{
                "url": "https://publisher.example/story",
                "final_url": "https://publisher.example/story",
                "navigations": [],
                "html": "",
                "error": "blocked by a bot challenge (not bypassed)"
            }]
        })
        .to_string();
        let err = parse_render_output(&stdout, "https://publisher.example/story").unwrap_err();
        assert_eq!(
            octos_research::ReadError::from_render_error(&err).reason,
            octos_research::ReadFailure::BotChallenge
        );
    }

    #[tokio::test]
    async fn should_not_consult_robots_txt_by_default() {
        if std::env::var(octos_research::RESPECT_ROBOTS_ENV).is_ok() {
            return; // operator setting present in this environment
        }
        let r = Reader::new(false);
        let hits = vec![SearchHit {
            url: "https://news.google.com/rss/articles/CBMi?oc=5".into(),
            provider: "google_news_rss".into(),
            ..Default::default()
        }];
        // No robots.txt request: every hit stays readable (Google News
        // links included), and nothing is recorded as a robots skip.
        let (ok, denied) = r.robots_partition(hits).await;
        assert_eq!(ok.len(), 1);
        assert!(denied.is_empty());
        assert!(!r.inner.respects_robots());
        assert_eq!(r.inner.robots_origins_requested(), 0);
    }

    #[test]
    fn should_not_plan_serp_scrapers_without_opt_in() {
        for news in [true, false] {
            let order = auto_plan(news, false);
            assert!(
                !order.contains(&Provider::DuckDuckGo) && !order.contains(&Provider::BingBrowser),
                "no search-results scraping in the default order: {order:?}"
            );
        }
        // The metasearch is the first free provider for every category
        // (GDELT runs inside it), unless OCTOS_METASEARCH=0.
        if octos_research::metasearch::enabled(|k| std::env::var(k).ok()) {
            assert_eq!(auto_plan(true, false)[0], Provider::Metasearch);
            assert_eq!(auto_plan(false, false)[0], Provider::Metasearch);
        } else {
            assert_eq!(auto_plan(true, false)[0], Provider::Gdelt);
        }
    }

    #[test]
    fn should_plan_ddg_then_bing_with_opt_in() {
        let order = auto_plan(false, true);
        let ddg = order.iter().position(|p| *p == Provider::DuckDuckGo);
        let bing = order.iter().position(|p| *p == Provider::BingBrowser);
        if octos_research::metasearch::enabled(|k| std::env::var(k).ok()) {
            // The metasearch's own DuckDuckGo and Bing engines ask them.
            assert!(ddg.is_none() && bing.is_none(), "{order:?}");
        } else {
            assert!(ddg.is_some() && bing.is_some() && ddg < bing, "{order:?}");
        }
    }

    #[tokio::test]
    async fn should_refuse_scrapers_without_opt_in() {
        let mut o = Options::from_input(&input(serde_json::json!({"query": "q"})), now()).unwrap();
        o.allow_serp_scrape = false;
        for p in [Provider::DuckDuckGo, Provider::BingBrowser] {
            let r = run_provider(&o, p, "q", None, 3).await;
            assert_eq!(r.err().as_deref(), Some("disabled"), "{p:?}");
        }
        // An explicit request is refused too, with the flag named.
        let out = search_round(&o, Some("duckduckgo"), "q", None, 3).await;
        assert!(
            out.errors
                .iter()
                .any(|e| e.contains(octos_research::SERP_SCRAPE_ENV)),
            "{:?}",
            out.errors
        );
        assert!(!out.tried.iter().any(|t| t == "duckduckgo"));
    }
}
