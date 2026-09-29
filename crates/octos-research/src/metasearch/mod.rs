//! octos metasearch: a Rust core that fans a query out to small search
//! engines written as sandboxed OctoScript scripts, then merges and ranks
//! what they return.
//!
//! Clean-room: every engine is written from its provider's public API
//! documentation (listed in its manifest's `docs_url`). Engines use official
//! APIs, open datasets or published feeds; no search-results page is
//! scraped. Requests carry the identifiable octos User-Agent, respect each
//! provider's published rate limit per host, honour `Retry-After`, and
//! revalidate cached responses with ETag / Last-Modified. robots.txt is
//! checked only when the operator turns it on ([`crate::RESPECT_ROBOTS_ENV`]):
//! an octos agent acts for one person.
//!
//! An engine is `engines/<id>/manifest.json` + `engine.octoscript` with:
//!
//! ```text
//! fn build_request(query, opts) -> {url, method, headers, body}
//! fn parse_response(response, opts) -> [item] | {items, backoff, error}
//!     response = {status, headers, json, body}
//! ```
//!
//! The script reaches only the hosts its manifest declares, through the
//! host-installed `net` module (see [`sandbox`]); the core performs the HTTP
//! request. Keys stay in the host: the core attaches them after the script
//! has built the request.

pub mod http;
pub mod manifest;
pub mod merge;
pub mod registry;
pub mod sandbox;
pub mod topic;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::date::Since;
use crate::filter::{DomainCap, Filters};
use crate::item::{ItemKind, SearchHit, SkippedUrl};
use crate::robots::RobotsCache;
use crate::{lang, urls};

#[cfg(feature = "http")]
pub use http::ReqwestFetch;
pub use http::{Fetch, FetchFuture, HttpRequest, HttpResponse};
pub use manifest::EngineManifest;
pub use merge::MetaItem;
pub use registry::{Engine, Registry};

use http::{CacheLookup, HostGate, ResponseCache};
use manifest::KeyAuth;
use merge::{RankOptions, RankedHit};
use sandbox::SandboxEngine;

/// Provider id of the metasearch in the octos provider chain.
pub const PROVIDER_ID: &str = "metasearch";

/// Environment variable that turns the metasearch off (`0`/`false`/`no`).
pub const METASEARCH_ENV: &str = "OCTOS_METASEARCH";

/// Contact address for polite pools (OpenAlex `mailto`). Optional.
pub const CONTACT_ENV: &str = "OCTOS_RESEARCH_CONTACT";

/// Directory with extra engines (see [`Registry::load_dir`]).
pub const ENGINES_DIR_ENV: &str = "OCTOS_METASEARCH_ENGINES";

/// Pins file for [`ENGINES_DIR_ENV`] engines (`{"id": "sha256:..."}`); must
/// live outside that directory.
pub const PINS_ENV: &str = "OCTOS_METASEARCH_PINS";

/// Lets a pinned directory engine replace a built-in with the same id
/// (`1`/`true`/`yes`; off by default).
pub const ALLOW_OVERRIDE_ENV: &str = "OCTOS_METASEARCH_ALLOW_OVERRIDE";

/// Default [`SearchRequest::straggler_grace`].
pub const DEFAULT_STRAGGLER_GRACE: Duration = Duration::from_secs(2);

/// Whether the metasearch is enabled (default on).
pub fn enabled(lookup: impl Fn(&str) -> Option<String>) -> bool {
    !lookup(METASEARCH_ENV).is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Host configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Engine id → API key. Never passed to scripts.
    pub keys: BTreeMap<String, String>,
    /// Engine id → setting → value (e.g. `mastodon.instance`).
    pub settings: BTreeMap<String, BTreeMap<String, String>>,
    /// Engines to leave out.
    pub disabled: Vec<String>,
    /// Engines marked `disabled_by_default` that the host turns on.
    pub enabled: Vec<String>,
    /// Contact address for polite pools.
    pub contact: Option<String>,
    /// First suspension after an engine error; doubles per consecutive
    /// error up to `backoff_max`.
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// Consecutive timeouts before an engine is suspended (for
    /// `backoff_base`, doubling like errors when it happens again before the
    /// engine answers).
    pub timeouts_before_suspend: u32,
    /// Check robots.txt for engines whose manifest sets `robots` (operator
    /// setting, off by default; see [`crate::RESPECT_ROBOTS_ENV`]).
    pub respect_robots: bool,
    /// Run engines whose manifest sets `results_page` (search engines' own
    /// results pages). On by default; see [`crate::SERP_SCRAPE_ENV`].
    pub results_pages: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keys: BTreeMap::new(),
            settings: BTreeMap::new(),
            disabled: Vec::new(),
            enabled: Vec::new(),
            contact: None,
            backoff_base: Duration::from_secs(30),
            backoff_max: Duration::from_secs(15 * 60),
            timeouts_before_suspend: 3,
            respect_robots: false,
            results_pages: true,
        }
    }
}

impl Config {
    /// Keys from each engine's `key_env`, settings from
    /// `OCTOS_METASEARCH_<ENGINE>_<SETTING>`, contact from
    /// [`CONTACT_ENV`]. `extra_keys` (engine id → key, e.g. a profile's
    /// provider keys) win over the environment.
    pub fn from_env(
        registry: &Registry,
        lookup: impl Fn(&str) -> Option<String>,
        extra_keys: &BTreeMap<String, String>,
    ) -> Self {
        let mut c = Self::default();
        let nonempty = |k: &str| lookup(k).filter(|v| !v.trim().is_empty());
        for e in registry.engines() {
            let m = &e.manifest;
            let key = extra_keys
                .get(&m.id)
                .cloned()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| m.key_env.as_deref().and_then(nonempty));
            if let Some(k) = key {
                c.keys.insert(m.id.clone(), k.trim().to_string());
            }
            for name in m.settings.keys() {
                let var = format!(
                    "OCTOS_METASEARCH_{}_{}",
                    m.id.to_ascii_uppercase(),
                    name.to_ascii_uppercase()
                );
                if let Some(v) = nonempty(&var) {
                    c.settings
                        .entry(m.id.clone())
                        .or_default()
                        .insert(name.clone(), v.trim().to_string());
                }
            }
        }
        c.contact = nonempty(CONTACT_ENV).filter(|v| v.contains('@'));
        c.respect_robots = crate::respect_robots(&lookup);
        c.results_pages = crate::serp_scrape_allowed(&lookup);
        c
    }
}

/// One search.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    /// Query per language (BCP-47 tag or primary subtag → query), used for
    /// calls in that language instead of `query`. Lets a caller search each
    /// language in its own words ("AI regulation" / "人工智能 监管").
    pub query_by_lang: BTreeMap<String, String>,
    /// Normalized BCP-47 tags to search in; empty = engine default.
    pub langs: Vec<String>,
    /// ISO 3166-1 alpha-2.
    pub region: Option<String>,
    pub since: Option<Since>,
    /// 1-based page.
    pub page: u32,
    /// Results asked of each engine.
    pub count: usize,
    /// Most merged items to return.
    pub limit: usize,
    /// One of [`manifest::CATEGORIES`].
    pub category: String,
    /// Domain allow/deny lists and the per-domain cap (its `langs` and
    /// `since` are taken from this request).
    pub filters: Filters,
    /// Only these engines (ids), if set.
    pub engines: Option<Vec<String>>,
    /// Overall deadline for the whole fan-out.
    pub deadline: Duration,
    /// Soft deadline: once an engine has answered with results and at most
    /// a quarter of the calls (at least one) are still running, those get
    /// this much longer; then they are dropped and reported as `timeout`.
    /// One slow provider (GDELT answers its 429 only after ~10 s) no longer
    /// holds up results the others already gave. `None` waits for every
    /// call (up to its own timeout and [`Self::deadline`]).
    pub straggler_grace: Option<Duration>,
    pub now: DateTime<Utc>,
}

impl SearchRequest {
    pub fn new(query: &str, category: &str) -> Self {
        Self {
            query: query.trim().to_string(),
            query_by_lang: BTreeMap::new(),
            langs: Vec::new(),
            region: None,
            since: None,
            page: 1,
            count: 10,
            limit: 30,
            category: category.to_string(),
            filters: Filters::default(),
            engines: None,
            deadline: Duration::from_secs(25),
            straggler_grace: Some(DEFAULT_STRAGGLER_GRACE),
            now: Utc::now(),
        }
    }

    /// The query for a call in `lang` (see [`lang::query_for`]).
    pub fn query_for(&self, lang: Option<&str>) -> &str {
        lang::query_for(&self.query, &self.query_by_lang, lang)
    }
}

/// How one engine call went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineStatus {
    Ok,
    Empty,
    Error,
    Timeout,
    /// Skipped: suspended after earlier errors or a `Retry-After`.
    Suspended,
    /// Skipped: its host's rate limit would not allow a call before the
    /// deadline.
    RateLimited,
    /// Skipped: robots.txt disallows the request.
    Robots,
    /// The site answered with a bot challenge (CAPTCHA, "unusual traffic").
    /// `challenge_url` is for the person to open and solve in their browser;
    /// it is never worked around.
    Challenge,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineReport {
    pub engine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    pub status: EngineStatus,
    pub hits: usize,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Served from cache (fresh or revalidated with a 304).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
    /// For [`EngineStatus::Challenge`]: the page for the person to open and
    /// solve in their browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenge_url: Option<String>,
}

/// Result of one search.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchResponse {
    pub items: Vec<MetaItem>,
    pub engines: Vec<EngineReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedUrl>,
    /// E.g. that key-less general search is thin, and how to widen it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl SearchResponse {
    /// Engines that returned at least one kept item.
    pub fn used_engines(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for i in &self.items {
            for e in &i.engines {
                if !out.contains(e) {
                    out.push(e.clone());
                }
            }
        }
        out
    }

    pub fn hits(&self) -> Vec<SearchHit> {
        self.items.iter().map(MetaItem::to_hit).collect()
    }
}

#[derive(Debug, Default, Clone)]
struct Health {
    errors: u32,
    timeouts: u32,
    suspended_until: Option<Instant>,
}

/// Politeness state: per-host slots, the response cache, robots.txt and
/// engine health. Share one per process so every caller respects the same
/// rate limits (see [`State::process_wide`]).
#[derive(Clone, Default)]
pub struct State {
    gate: HostGate,
    cache: ResponseCache,
    robots: RobotsCache,
    health: Arc<Mutex<HashMap<String, Health>>>,
}

impl State {
    /// The state shared by every metasearch in this process.
    pub fn process_wide() -> State {
        static S: std::sync::OnceLock<State> = std::sync::OnceLock::new();
        S.get_or_init(State::default).clone()
    }
}

struct Inner {
    registry: Registry,
    fetch: Arc<dyn Fetch>,
    config: Config,
    gate: HostGate,
    cache: ResponseCache,
    robots: RobotsCache,
    health: Arc<Mutex<HashMap<String, Health>>>,
}

/// The metasearch. Cheap to clone; clones share rate limits, cache and
/// engine health.
#[derive(Clone)]
pub struct Metasearch {
    inner: Arc<Inner>,
}

/// One planned engine call.
struct Call<'a> {
    engine: &'a Engine,
    /// Languages this call covers (empty = engine default).
    langs: Vec<String>,
}

impl Metasearch {
    /// A metasearch with its own politeness state (tests, tools).
    pub fn new(registry: Registry, fetch: Arc<dyn Fetch>, config: Config) -> Self {
        Self::with_state(registry, fetch, config, State::default())
    }

    pub fn with_state(
        registry: Registry,
        fetch: Arc<dyn Fetch>,
        config: Config,
        state: State,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                registry,
                fetch,
                config,
                gate: state.gate,
                cache: state.cache,
                robots: state.robots,
                health: state.health,
            }),
        }
    }

    /// Built-in engines plus pinned engines from [`ENGINES_DIR_ENV`], keys
    /// and settings from the environment (and `extra_keys`), sharing the
    /// process-wide politeness state.
    pub fn from_env(fetch: Arc<dyn Fetch>, extra_keys: &BTreeMap<String, String>) -> Self {
        let lookup = |k: &str| std::env::var(k).ok();
        let mut registry = Registry::builtin();
        if let Some(dir) = lookup(ENGINES_DIR_ENV).filter(|d| !d.trim().is_empty()) {
            let dir = std::path::Path::new(dir.trim());
            let pins = match lookup(PINS_ENV).filter(|p| !p.trim().is_empty()) {
                Some(p) => {
                    registry::read_pins(std::path::Path::new(p.trim()), dir).unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "metasearch pins not loaded");
                        BTreeMap::new()
                    })
                }
                None => BTreeMap::new(),
            };
            let allow_override = lookup(ALLOW_OVERRIDE_ENV).is_some_and(|v| {
                matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
            });
            registry.load_dir(dir, &pins, allow_override);
            for r in &registry.rejected {
                tracing::warn!(path = %r.path, reason = %r.reason, "metasearch engine rejected");
            }
        }
        let config = Config::from_env(&registry, lookup, extra_keys);
        Self::with_state(registry, fetch, config, State::process_wide())
    }

    pub fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    /// Engines that would run for `category` (enabled, keyed if needed).
    pub fn engines_for(&self, category: &str) -> Vec<&EngineManifest> {
        self.inner
            .registry
            .engines()
            .map(|e| &e.manifest)
            .filter(|m| m.serves(category) && self.is_enabled(m))
            .collect()
    }

    fn is_enabled(&self, m: &EngineManifest) -> bool {
        let c = &self.inner.config;
        if c.disabled.contains(&m.id) {
            return false;
        }
        if m.disabled_by_default && !c.enabled.contains(&m.id) {
            return false;
        }
        if m.results_page && !c.results_pages {
            return false;
        }
        !m.needs_key || c.keys.contains_key(&m.id)
    }

    /// Whether key-less general search is all this host has.
    fn general_is_thin(&self) -> bool {
        !self.engines_for("general").iter().any(|m| m.needs_key)
    }

    fn plan<'a>(&'a self, req: &SearchRequest) -> Vec<Call<'a>> {
        let mut calls = Vec::new();
        for e in self.inner.registry.engines() {
            let m = &e.manifest;
            if !m.serves(&req.category) || !self.is_enabled(m) {
                continue;
            }
            if let Some(only) = &req.engines {
                if !only.iter().any(|id| id == &m.id) {
                    continue;
                }
            }
            if req.langs.is_empty() {
                calls.push(Call {
                    engine: e,
                    langs: Vec::new(),
                });
                continue;
            }
            let supported: Vec<String> = req
                .langs
                .iter()
                .filter(|l| m.supports_lang(l))
                .cloned()
                .collect();
            if supported.is_empty() {
                continue;
            }
            // One call can cover several languages only when they share a
            // query.
            let own_queries = supported
                .iter()
                .any(|l| req.query_for(Some(l)) != req.query);
            if m.multi_language && !own_queries {
                calls.push(Call {
                    engine: e,
                    langs: supported,
                });
            } else {
                for l in supported {
                    calls.push(Call {
                        engine: e,
                        langs: vec![l],
                    });
                }
            }
        }
        calls
    }

    /// Run a search: plan, fan out in parallel, filter, merge, rank.
    pub async fn search(&self, req: &SearchRequest) -> SearchResponse {
        let started = Instant::now();
        let calls = self.plan(req);
        let results = self.fan_out(&calls, req, started).await;

        let mut filters = req.filters.clone();
        filters.langs = req.langs.clone();
        filters.since = req.since.clone();
        let mut ranked = Vec::new();
        let mut reports = Vec::new();
        let mut skipped = Vec::new();
        for (call, (mut report, hits)) in calls.iter().zip(results) {
            // Engines that list rather than search: only hits about the
            // query count.
            let matcher = call
                .engine
                .manifest
                .query_match
                .then(|| topic::QueryMatcher::new(call_query(call, req)));
            for (position, rh) in hits.into_iter().enumerate() {
                if let Some(m) = &matcher {
                    if !m.matches(&format!("{} {}", rh.hit.title, rh.hit.snippet)) {
                        report.hits = report.hits.saturating_sub(1);
                        if report.hits == 0 && report.status == EngineStatus::Ok {
                            report.status = EngineStatus::Empty;
                        }
                        skipped.push(SkippedUrl::new(rh.hit.url, "query_mismatch".to_string()));
                        continue;
                    }
                }
                match filters.check(
                    rh.hit.domain_url(),
                    rh.hit.lang.as_deref(),
                    rh.hit.published.as_deref(),
                ) {
                    Ok(()) => ranked.push(RankedHit { position, ..rh }),
                    Err(reason) => skipped.push(SkippedUrl::new(rh.hit.url, reason.to_string())),
                }
            }
            reports.push(report);
        }
        let half_life_days = if req.category == "news" { 3.0 } else { 365.0 };
        let merged = merge::merge(
            ranked,
            &req.category,
            RankOptions {
                now: req.now,
                half_life_days,
            },
        );
        let mut cap = DomainCap::new(filters.max_per_domain);
        let mut items = Vec::new();
        for item in merged {
            let domain_url = match (&item.source_url, urls::domain_of(&item.url)) {
                (Some(s), Some(d)) if d == "news.google.com" => s.clone(),
                _ => item.url.clone(),
            };
            if !cap.admit(&domain_url) {
                skipped.push(SkippedUrl::new(item.url, "per_domain_cap".to_string()));
                continue;
            }
            if items.len() < req.limit {
                items.push(item);
            }
        }
        let note = (req.category == "general" && self.general_is_thin()).then(|| {
            "The metasearch's key-less general engines are Wikipedia and Wikidata. Web \
             results come from results-page search (DuckDuckGo, Bing; on unless \
             OCTOS_ALLOW_SERP_SCRAPE=0), a Brave Search key (BRAVE_API_KEY) or a \
             self-hosted SearXNG (SEARXNG_URL)."
                .to_string()
        });
        SearchResponse {
            items,
            engines: reports,
            skipped,
            note,
        }
    }

    /// Run every call in parallel. With a [`SearchRequest::straggler_grace`],
    /// calls still running that long after the soft deadline started (an
    /// engine answered with results, and at most a quarter of the calls, at
    /// least one, are left) are dropped and reported as `timeout`. Reports
    /// keep the plan's order.
    async fn fan_out(
        &self,
        calls: &[Call<'_>],
        req: &SearchRequest,
        started: Instant,
    ) -> Vec<(EngineReport, Vec<RankedHit>)> {
        use futures::stream::{FuturesUnordered, StreamExt};

        let mut out: Vec<Option<(EngineReport, Vec<RankedHit>)>> =
            calls.iter().map(|_| None).collect();
        let mut soft_deadline: Option<Instant> = None;
        {
            let mut running: FuturesUnordered<_> = calls
                .iter()
                .enumerate()
                .map(|(i, c)| async move { (i, self.run_call(c, req, started).await) })
                .collect();
            let mut done = 0usize;
            let mut answered = false;
            while done < calls.len() {
                let next = match soft_deadline {
                    Some(at) => match tokio::time::timeout_at(at, running.next()).await {
                        Ok(next) => next,
                        // Dropping `running` cancels the stragglers.
                        Err(_) => break,
                    },
                    None => running.next().await,
                };
                let Some((i, result)) = next else { break };
                answered |= !result.1.is_empty();
                out[i] = Some(result);
                done += 1;
                let left = calls.len() - done;
                if let Some(grace) = req.straggler_grace {
                    if soft_deadline.is_none()
                        && answered
                        && left > 0
                        && left <= (calls.len() / 4).max(1)
                    {
                        soft_deadline = Some(Instant::now() + grace);
                    }
                }
            }
        }
        let grace = req.straggler_grace.unwrap_or_default();
        out.into_iter()
            .zip(calls)
            .map(|(r, call)| r.unwrap_or_else(|| self.dropped(call, grace, started)))
            .collect()
    }

    /// The report for a call dropped at the soft deadline (counted as a
    /// timeout for the engine's health).
    fn dropped(
        &self,
        call: &Call<'_>,
        grace: Duration,
        started: Instant,
    ) -> (EngineReport, Vec<RankedHit>) {
        let m = &call.engine.manifest;
        self.record(&m.id, EngineStatus::Timeout, None);
        let report = EngineReport {
            engine: m.id.clone(),
            lang: (!call.langs.is_empty()).then(|| call.langs.join(",")),
            status: EngineStatus::Timeout,
            hits: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
            error: Some(format!(
                "still running {:.1}s after the other engines had answered; dropped (soft \
                 deadline)",
                grace.as_secs_f64()
            )),
            cached: false,
            challenge_url: None,
        };
        (report, Vec::new())
    }

    fn suspended(&self, id: &str) -> Option<Duration> {
        let h = self.inner.health.lock().unwrap_or_else(|p| p.into_inner());
        h.get(id)
            .and_then(|s| s.suspended_until)
            .map(|t| t.saturating_duration_since(Instant::now()))
            .filter(|d| !d.is_zero())
    }

    fn record(&self, id: &str, status: EngineStatus, retry_after: Option<Duration>) {
        let c = &self.inner.config;
        let mut map = self.inner.health.lock().unwrap_or_else(|p| p.into_inner());
        let h = map.entry(id.to_string()).or_default();
        match status {
            EngineStatus::Ok | EngineStatus::Empty => *h = Health::default(),
            EngineStatus::Error => {
                h.errors += 1;
                h.timeouts = 0;
                let factor = 2u32.saturating_pow(h.errors.saturating_sub(1).min(16));
                let backoff = c.backoff_base.saturating_mul(factor).min(c.backoff_max);
                let pause = retry_after.map_or(backoff, |r| r.max(backoff));
                h.suspended_until = Some(Instant::now() + pause);
            }
            EngineStatus::Timeout => {
                h.timeouts += 1;
                if h.timeouts >= c.timeouts_before_suspend {
                    // Repeated runs of timeouts back off like errors do.
                    h.errors += 1;
                    let factor = 2u32.saturating_pow(h.errors.saturating_sub(1).min(16));
                    let backoff = c.backoff_base.saturating_mul(factor).min(c.backoff_max);
                    h.suspended_until = Some(Instant::now() + backoff);
                    h.timeouts = 0;
                }
            }
            _ => {}
        }
    }

    fn opts_json(&self, call: &Call<'_>, req: &SearchRequest) -> Value {
        let m = &call.engine.manifest;
        let langs: Vec<Value> = call
            .langs
            .iter()
            .map(|t| {
                let region = t
                    .split('-')
                    .skip(1)
                    .find(|s| s.len() == 2)
                    .map(|s| s.to_ascii_uppercase());
                json!({
                    "tag": t,
                    "primary": lang::primary(t),
                    "region": region,
                    "name": lang::gdelt_sourcelang(t),
                })
            })
            .collect();
        let since = req.since.as_ref().map(|s| {
            let hours = s.effective_span(req.now).num_hours().max(1);
            json!({
                "iso": s.cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "date": s.cutoff.format("%Y-%m-%d").to_string(),
                "compact": s.cutoff.format("%Y%m%d%H%M%S").to_string(),
                "unix": s.cutoff.timestamp(),
                "hours": hours,
                "relative": s.span.is_some(),
            })
        });
        let mut settings = serde_json::Map::new();
        for (name, setting) in &m.settings {
            let v = self
                .inner
                .config
                .settings
                .get(&m.id)
                .and_then(|s| s.get(name))
                .cloned()
                .or_else(|| setting.default.clone());
            settings.insert(name.clone(), json!(v));
        }
        let now = json!({
            "iso": req.now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "date": req.now.format("%Y-%m-%d").to_string(),
            "compact": req.now.format("%Y%m%d%H%M%S").to_string(),
            "unix": req.now.timestamp(),
        });
        let query = call_query(call, req);
        json!({
            "query": query,
            "now": now,
            "langs": langs,
            "lang": langs.first().cloned(),
            "region": req.region,
            "since": since,
            "page": req.page.max(1),
            "count": req.count.clamp(1, 50),
            "category": req.category,
            "contact": self.inner.config.contact,
            "settings": settings,
        })
    }

    fn allowed_hosts(&self, m: &EngineManifest) -> Vec<String> {
        let mut hosts = m.hosts.clone();
        for (name, setting) in &m.settings {
            if !setting.host {
                continue;
            }
            let v = self
                .inner
                .config
                .settings
                .get(&m.id)
                .and_then(|s| s.get(name))
                .cloned()
                .or_else(|| setting.default.clone());
            if let Some(h) = v.map(|h| h.trim().to_ascii_lowercase()) {
                if manifest::is_host_name(&h) && !hosts.contains(&h) {
                    hosts.push(h);
                }
            }
        }
        hosts
    }

    async fn run_call(
        &self,
        call: &Call<'_>,
        req: &SearchRequest,
        started: Instant,
    ) -> (EngineReport, Vec<RankedHit>) {
        let m = &call.engine.manifest;
        let t0 = Instant::now();
        let lang = (!call.langs.is_empty()).then(|| call.langs.join(","));
        let report = |status, hits, error: Option<String>, cached| EngineReport {
            engine: m.id.clone(),
            lang: lang.clone(),
            status,
            hits,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            error,
            cached,
            challenge_url: None,
        };
        if let Some(left) = self.suspended(&m.id) {
            let msg = format!(
                "suspended for {}s after earlier errors",
                left.as_secs().max(1)
            );
            return (
                report(EngineStatus::Suspended, 0, Some(msg), false),
                Vec::new(),
            );
        }
        match self.call_engine(call, req, started).await {
            Ok((hits, cached, partial)) => {
                let status = if hits.is_empty() {
                    EngineStatus::Empty
                } else {
                    EngineStatus::Ok
                };
                self.record(&m.id, status, None);
                (report(status, hits.len(), partial, cached), hits)
            }
            Err(CallError::Timeout) => {
                self.record(&m.id, EngineStatus::Timeout, None);
                let msg = format!("no response within {}s", m.timeout_secs);
                (
                    report(EngineStatus::Timeout, 0, Some(msg), false),
                    Vec::new(),
                )
            }
            Err(CallError::Skip(status, msg)) => (report(status, 0, Some(msg), false), Vec::new()),
            Err(CallError::Challenge(reason, url)) => {
                // Back off like an error so the site isn't asked again at
                // once; the person may clear it in their browser meanwhile.
                self.record(&m.id, EngineStatus::Error, None);
                let msg = format!("challenge ({reason}): open the page to continue");
                let mut r = report(EngineStatus::Challenge, 0, Some(msg), false);
                r.challenge_url = Some(url);
                (r, Vec::new())
            }
            Err(CallError::Failed(msg, retry_after)) => {
                self.record(&m.id, EngineStatus::Error, retry_after);
                tracing::warn!(engine = %m.id, error = %msg, "metasearch engine error");
                (report(EngineStatus::Error, 0, Some(msg), false), Vec::new())
            }
        }
    }

    async fn call_engine(
        &self,
        call: &Call<'_>,
        req: &SearchRequest,
        started: Instant,
    ) -> Result<(Vec<RankedHit>, bool, Option<String>), CallError> {
        let e = call.engine;
        let m = &e.manifest;
        let hosts = self.allowed_hosts(m);
        let sb = SandboxEngine {
            id: &m.id,
            source: &e.source,
            allowed_hosts: &hosts,
            allow_http: m.allow_http,
        };
        let opts = self.opts_json(call, req);
        let query = call_query(call, req);
        let requests = sandbox::build_request(&sb, query, &opts)
            .map_err(|err| CallError::Failed(err, None))?;
        if requests.is_empty() {
            return Ok((Vec::new(), false, None));
        }
        if requests.len() > m.max_requests {
            return Err(CallError::Failed(
                format!(
                    "{}: built {} requests, manifest allows {}",
                    m.id,
                    requests.len(),
                    m.max_requests
                ),
                None,
            ));
        }

        // One request per feed/page; each keeps its own host's spacing.
        let results = futures::future::join_all(
            requests
                .iter()
                .map(|sreq| self.fetch_and_parse(&sb, m, sreq, &opts, req, started)),
        )
        .await;
        let single = results.len() == 1;
        let mut lists = Vec::new();
        let mut all_cached = true;
        let mut first_err = None;
        let mut failed = 0usize;
        for (sreq, r) in requests.iter().zip(results) {
            match r {
                Ok((items, cached)) => {
                    all_cached &= cached;
                    lists.push(items);
                }
                Err(err) => {
                    failed += 1;
                    if !single {
                        tracing::warn!(engine = %m.id, url = %sreq.url, "metasearch request failed");
                    }
                    first_err.get_or_insert(err);
                }
            }
        }
        if lists.is_empty() {
            return Err(first_err.unwrap_or(CallError::Failed("no response".into(), None)));
        }
        // Some requests of a multi-request engine failed: say so in the
        // report instead of only in the log.
        let partial = (failed > 0).then(|| {
            let why = match &first_err {
                Some(CallError::Failed(msg, _)) => msg.clone(),
                Some(CallError::Timeout) => "timed out".to_string(),
                Some(CallError::Skip(_, msg)) => msg.clone(),
                Some(CallError::Challenge(reason, _)) => format!("challenge ({reason})"),
                None => String::new(),
            };
            format!("{failed} of {} requests failed: {why}", requests.len())
        });
        // Interleave so no single feed/page takes every top position.
        let mut items = Vec::new();
        let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
        for k in 0..longest {
            for list in &lists {
                if let Some(v) = list.get(k) {
                    items.push(v.clone());
                }
            }
        }
        let default_lang = (call.langs.len() == 1).then(|| call.langs[0].clone());
        let hits = items
            .iter()
            .filter_map(|v| normalize_item(v, &m.id, m.kind, default_lang.as_deref()))
            .take(req.count.max(1))
            .map(|hit| RankedHit {
                engine: m.id.clone(),
                position: 0,
                weight: m.weight,
                hit,
            })
            .collect();
        Ok((hits, all_cached, partial))
    }

    /// Perform one request an engine built (robots when enabled, cache,
    /// host spacing, key, status handling) and run `parse_response` on it.
    async fn fetch_and_parse(
        &self,
        sb: &SandboxEngine<'_>,
        m: &EngineManifest,
        sreq: &sandbox::ScriptRequest,
        opts: &Value,
        req: &SearchRequest,
        started: Instant,
    ) -> Result<(Vec<Value>, bool), CallError> {
        let url =
            url::Url::parse(&sreq.url).map_err(|err| CallError::Failed(err.to_string(), None))?;
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        // robots.txt: only when the operator turned checks on.
        let mut interval = m.min_interval();
        if m.robots && self.inner.config.respect_robots {
            let fetch = self.inner.fetch.clone();
            let decision = self
                .inner
                .robots
                .check(&sreq.url, crate::AGENT_TOKEN, |robots_url| async move {
                    let r = fetch
                        .fetch(HttpRequest {
                            method: "GET".into(),
                            url: robots_url,
                            headers: vec![("user-agent".into(), crate::USER_AGENT.into())],
                            body: None,
                            timeout: Duration::from_secs(8),
                        })
                        .await;
                    match r {
                        Ok(r) => (Some(r.status), r.body),
                        Err(_) => (None, String::new()),
                    }
                })
                .await;
            if !decision.allowed {
                return Err(CallError::Skip(
                    EngineStatus::Robots,
                    format!("robots.txt: {}", decision.reason),
                ));
            }
            if let Some(d) = decision.crawl_delay {
                interval = interval.max(d.min(Duration::from_secs(30)));
            }
        }

        // Cache (keyed before the key is attached).
        let cache_key = format!("{} {}", sreq.method, sreq.url);
        let cacheable = sreq.method == "GET";
        let mut conditional = Vec::new();
        let mut cached_response = None;
        if cacheable {
            match self.inner.cache.lookup(&cache_key) {
                CacheLookup::Fresh(r) => cached_response = Some(r),
                CacheLookup::Revalidate(h) => conditional = h,
                CacheLookup::Miss => {}
            }
        }

        let (response, cached) = match cached_response {
            Some(r) => (r, true),
            None => {
                // Politeness: wait for this host's slot, unless that would
                // miss the deadline.
                let remaining = req.deadline.saturating_sub(started.elapsed());
                let hint = self.inner.gate.wait_hint(&host);
                if hint + Duration::from_millis(250) >= remaining {
                    return Err(CallError::Skip(
                        EngineStatus::RateLimited,
                        format!("{host} rate limit: next slot in {}ms", hint.as_millis()),
                    ));
                }
                let wait = self.inner.gate.reserve(&host, interval);
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
                let mut http = HttpRequest {
                    method: sreq.method.clone(),
                    url: sreq.url.clone(),
                    headers: sreq.headers.clone(),
                    body: sreq.body.clone(),
                    timeout: Duration::from_secs(m.timeout_secs),
                };
                http.headers
                    .push(("user-agent".into(), crate::USER_AGENT.into()));
                http.headers.extend(conditional.iter().cloned());
                self.attach_key(m, &mut http)?;
                let timeout = Duration::from_secs(m.timeout_secs)
                    .min(req.deadline.saturating_sub(started.elapsed()));
                let call = if sreq.render {
                    self.inner.fetch.render(http)
                } else {
                    self.inner.fetch.fetch(http)
                };
                let r = tokio::time::timeout(timeout, call)
                    .await
                    .map_err(|_| CallError::Timeout)?
                    .map_err(|err| CallError::Failed(err, None))?;
                if r.status == 304 && !conditional.is_empty() {
                    match self.inner.cache.revalidated(&cache_key) {
                        Some(stored) => (stored, true),
                        None => {
                            return Err(CallError::Failed(
                                "304 without a cached copy".into(),
                                None,
                            ));
                        }
                    }
                } else {
                    (r, false)
                }
            }
        };

        if matches!(response.status, 429 | 503) {
            let retry = response
                .header("retry-after")
                .and_then(|v| http::parse_retry_after(v, Utc::now()));
            if let Some(d) = retry {
                self.inner.gate.block(&host, d);
            }
            let msg = match retry {
                Some(d) => format!("HTTP {} (retry after {}s)", response.status, d.as_secs()),
                None => format!("HTTP {}", response.status),
            };
            return Err(CallError::Failed(msg, retry));
        }
        if !(200..300).contains(&response.status) {
            let head: String = response.body.chars().take(160).collect();
            return Err(CallError::Failed(
                format!("HTTP {}: {}", response.status, head.trim()),
                None,
            ));
        }
        let parsed = sandbox::parse_response(
            sb,
            opts,
            &sreq.url,
            response.status,
            &response.headers,
            &response.body,
        )
        .map_err(|err| CallError::Failed(err, None))?;
        if let Some(b) = parsed.backoff {
            self.inner.gate.block(&host, b);
        }
        if let Some(reason) = parsed.challenge {
            return Err(CallError::Challenge(reason, sreq.url.clone()));
        }
        if let Some(err) = parsed.error {
            return Err(CallError::Failed(err, parsed.backoff));
        }
        // Only responses the engine accepted are reused.
        if !cached && cacheable {
            self.inner
                .cache
                .store(&cache_key, &response, Duration::from_secs(m.cache_ttl_secs));
        }
        Ok((parsed.items, cached))
    }

    fn attach_key(&self, m: &EngineManifest, http: &mut HttpRequest) -> Result<(), CallError> {
        let (Some(auth), Some(key)) = (&m.auth, self.inner.config.keys.get(&m.id)) else {
            return if m.needs_key {
                Err(CallError::Skip(
                    EngineStatus::Error,
                    "no API key configured".into(),
                ))
            } else {
                Ok(())
            };
        };
        match auth {
            KeyAuth::Header { name } => http.headers.push((name.to_ascii_lowercase(), key.clone())),
            KeyAuth::Bearer => http
                .headers
                .push(("authorization".into(), format!("Bearer {key}"))),
            KeyAuth::Query { name } => {
                let mut u = url::Url::parse(&http.url)
                    .map_err(|err| CallError::Failed(err.to_string(), None))?;
                u.query_pairs_mut().append_pair(name, key);
                http.url = u.to_string();
            }
        }
        Ok(())
    }
}

/// The query a call searches: the language's own query for a
/// one-language call, else the shared one.
fn call_query<'r>(call: &Call<'_>, req: &'r SearchRequest) -> &'r str {
    req.query_for(
        call.langs
            .first()
            .map(String::as_str)
            .filter(|_| call.langs.len() == 1),
    )
}

enum CallError {
    Timeout,
    Skip(EngineStatus, String),
    Failed(String, Option<Duration>),
    /// A bot challenge: (reason, the page URL).
    Challenge(String, String),
}

/// Validate and normalize one item a script returned. `kind` is the
/// engine's default; an item's own valid `kind` wins.
fn normalize_item(
    v: &Value,
    engine: &str,
    kind: ItemKind,
    default_lang: Option<&str>,
) -> Option<SearchHit> {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(|x| x.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|x| !x.is_empty())
    };
    let url = s("url")?;
    let parsed = url::Url::parse(&url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    let mut snippet = s("snippet").unwrap_or_default();
    if snippet.chars().count() > 400 {
        snippet = snippet.chars().take(400).collect::<String>() + "…";
    }
    let published = match v.get("published") {
        Some(Value::String(p)) => crate::date::to_iso(p),
        Some(Value::Number(n)) => n
            .as_i64()
            .and_then(|t| DateTime::from_timestamp(t, 0))
            .map(|dt| crate::date::format_iso(dt, true)),
        _ => None,
    };
    // A BCP-47 tag, or an English language name ("Chinese", as GDELT
    // reports it).
    let lang = s("lang")
        .and_then(|l| lang::normalize(&l).or_else(|| lang::from_gdelt_name(&l).map(String::from)))
        .or_else(|| default_lang.map(String::from));
    let mut title = s("title").unwrap_or_default();
    if title.chars().count() > 300 {
        title = title.chars().take(300).collect::<String>() + "…";
    }
    Some(SearchHit {
        title: if title.is_empty() { url.clone() } else { title },
        url,
        snippet,
        source: s("source"),
        source_url: s("source_url"),
        lang,
        published,
        provider: engine.to_string(),
        kind: v
            .get("kind")
            .and_then(Value::as_str)
            .and_then(ItemKind::parse)
            .unwrap_or(kind),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests;
