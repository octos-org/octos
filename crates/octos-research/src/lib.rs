//! Research building blocks shared by the octos search tools (the built-in
//! `web_search` / `search` tools and the `deep-search` / `deep-crawl` skills).
//!
//! Policy (OctoSense ADR 0002, section 6): free structured sources first
//! (GDELT, Google News RSS), then a self-hosted SearXNG if one is configured,
//! then search API keys the person chose to add, then search engines'
//! results pages for general web search (on by default; an operator can turn
//! them off). Pages that will be cited are *read* (optionally rendered by a
//! real browser) at a polite rate, with an identifiable User-Agent. The
//! results-page engines search the way SearXNG does (the maintainer's
//! decision): Google's page for simple phones only answers a browser-like
//! client, which `metasearch::impersonate` presents for the engines that
//! name it. Nothing solves CAPTCHAs: a challenge suspends that engine and no
//! person is asked.
//!
//! This crate is deliberately network-free: it builds provider request URLs,
//! parses provider responses, filters and caps results, parses robots.txt and
//! extracts main text + metadata from HTML. Callers own the HTTP client (each
//! has its own SSRF-safe fetch path), which keeps every piece here testable
//! from fixtures.

pub mod access;
#[cfg(feature = "browser")]
pub mod browser;
pub mod date;
#[cfg(feature = "extract")]
pub mod extract;
pub mod filter;
pub mod item;
pub mod lang;
#[cfg(feature = "metasearch")]
pub mod metasearch;
#[cfg(feature = "fetch")]
pub mod net;
pub mod plan;
pub mod providers;
#[cfg(all(feature = "fetch", feature = "extract"))]
pub mod reader;
pub mod robots;
pub mod text;
pub mod throttle;
#[cfg(all(feature = "metasearch", feature = "extract"))]
pub mod toolbox;
pub mod urls;

pub use access::{ReadError, ReadFailure};
pub use filter::{DomainCap, Filters, OneOrMany};
pub use item::{ItemKind, ItemsDocument, ResearchItem, SearchHit, SkippedUrl, SummaryKind};
pub use plan::{Category, Provider};
pub use robots::{Robots, RobotsCache, RobotsStatus};
pub use throttle::HostThrottle;

/// robots.txt product token matched against `User-agent:` lines.
pub const AGENT_TOKEN: &str = "octos-research";

/// Identifiable User-Agent for every request the research tools make on
/// their own behalf (provider APIs, robots.txt, page reads). It names the
/// software and where to learn about it, instead of posing as a desktop
/// browser.
pub const USER_AGENT: &str = "octos-research/1.0 (+https://github.com/octos-org/octos)";

/// Environment variable for results-page search: the metasearch's engines
/// that read search engines' own pages (DuckDuckGo, Bing, Bing News, Brave,
/// Google). **On by default** (OctoSense ADR 0002 §6 as amended: search the
/// way SearXNG does, no person in the loop); set it to `0`/`false`/`no`/
/// `off` to turn it off. Most of these engines identify as octos. Google's
/// page answers only a browser-like client, so its engine is fetched with
/// the `legacy_mobile` client (a feature-phone User-Agent over a Chrome TLS
/// fingerprint, as SearXNG does; see `metasearch::impersonate`). No CAPTCHA
/// is solved: a challenge suspends that engine. Search engines' terms may
/// not allow automated queries (Google and Bing: high risk).
pub const SERP_SCRAPE_ENV: &str = "OCTOS_ALLOW_SERP_SCRAPE";

/// Earlier name of [`SERP_SCRAPE_ENV`], still honoured as an alias.
pub const BROWSER_SERP_ENV: &str = "OCTOS_ALLOW_BROWSER_SERP";

/// Operator setting that turns robots.txt checks **on** for the research
/// tools (`1`/`true`/`yes`). Default off: OctoSense agents are personal
/// assistants reading on behalf of one person (a product decision by the
/// maintainer). When off, robots.txt is never fetched or consulted; the
/// honest User-Agent, per-host spacing, 429/503 backoff, timeouts, size
/// caps and SSRF protections all still apply.
pub const RESPECT_ROBOTS_ENV: &str = "OCTOS_RESPECT_ROBOTS";

/// Whether robots.txt checks are enabled (env lookup injected for tests).
pub fn respect_robots(lookup: impl Fn(&str) -> Option<String>) -> bool {
    lookup(RESPECT_ROBOTS_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Person's-browser mode for engines that render pages in a real browser
/// (`google_cse`): `off` (default) | `auto` | `window` | `headless`. See
/// `octos_research::browser` (feature `browser`).
pub const BROWSER_ENV: &str = "OCTOS_BROWSER";

/// Shown with results whenever a search used the person's browser (and
/// logged once when the browser starts): what that means for their account
/// and how to turn it off.
pub const BROWSER_SEARCH_NOTICE: &str = "Some results were loaded in the octos browser profile \
     (~/.octos/browser-profile). If you signed in to Google there, those searches ran as your \
     Google account: results may be personalised and are saved to its search activity. Search \
     engines' terms may not allow automated queries. Set OCTOS_BROWSER=off to stop using the \
     browser.";

/// Operator switch for reading a page that plain HTTP was blocked on (a bot
/// challenge, 401 or 403) once in the configured browser renderer. Unset:
/// on (OctoSense ADR 0002 §6 as amended 2026-09-29: the maintainer decided
/// octos searches and reads the way SearXNG does, with no person in the
/// loop; a real browser may read a page a plain client was refused, and
/// nothing is solved or clicked). Set: on only for `1`/`true`/`yes`/`on`;
/// any other value turns it off, so a mistyped opt-out fails safe. Before
/// this switch (octos#2590 to #2637) a challenge over plain HTTP was never
/// retried in the browser; set it to `0` to keep that behaviour.
pub const READ_BLOCKED_IN_BROWSER_ENV: &str = "OCTOS_READ_BLOCKED_IN_BROWSER";

/// Whether a page blocked over plain HTTP may be read once in the browser
/// ([`READ_BLOCKED_IN_BROWSER_ENV`]; env lookup injected for tests).
pub fn read_blocked_in_browser(lookup: impl Fn(&str) -> Option<String>) -> bool {
    lookup(READ_BLOCKED_IN_BROWSER_ENV).is_none_or(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Environment variable naming a self-hosted SearXNG base URL
/// (e.g. `http://127.0.0.1:8888`).
pub const SEARXNG_URL_ENV: &str = "SEARXNG_URL";

/// Whether results-page search (see [`SERP_SCRAPE_ENV`]) is on.
/// Unset: on (the default, OctoSense ADR 0002 §6 amendment). Set: on only
/// for `1`/`true`/`yes`/`on`; any other value, including an empty or
/// unrecognised one, turns it **off**, so a mistyped opt-out fails safe.
/// If either [`SERP_SCRAPE_ENV`] or its alias [`BROWSER_SERP_ENV`] turns it
/// off, it is off. The env lookup is injected so tests never touch process
/// env.
pub fn serp_scrape_allowed(lookup: impl Fn(&str) -> Option<String>) -> bool {
    [SERP_SCRAPE_ENV, BROWSER_SERP_ENV].iter().all(|k| {
        lookup(k).is_none_or(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    })
}

/// One-time notice for hosts to log when results-page search runs only
/// because of the default (the variable is unset): it was off by default
/// before, so an upgrade changes behaviour. `None` when the operator set
/// the variable either way.
pub fn serp_scrape_default_notice(lookup: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    let unset = [SERP_SCRAPE_ENV, BROWSER_SERP_ENV]
        .iter()
        .all(|k| lookup(k).is_none());
    unset.then_some(
        "Results-page search (DuckDuckGo, Bing, Brave and Google results pages) is on by \
         default for general web results, the way SearXNG searches (OctoSense ADR 0002 \
         amendment). Google's page is fetched with a browser-like client (a feature-phone \
         User-Agent over a Chrome TLS fingerprint). Search engines' terms may not allow \
         automated queries (Google and Bing: high risk; DuckDuckGo: its robots.txt allows the \
         HTML page, its terms promise nothing). Set OCTOS_ALLOW_SERP_SCRAPE=0 to turn it off.",
    )
}

/// Message for a search where no provider returned anything: what was tried
/// and how to widen the search.
pub fn no_results_message(query: &str, tried: &[String]) -> String {
    let tried = if tried.is_empty() {
        "none (the query is not news-ish and no SearXNG or search API key is configured)"
            .to_string()
    } else {
        tried.join(", ")
    };
    format!(
        "No results for: {query}\n\nProviders tried: {tried}.\n\n\
         For more results on general queries, add a search API key (BRAVE_API_KEY, \
         SERPER_API_KEY, TAVILY_API_KEY, YDC_API_KEY or PERPLEXITY_API_KEY) or set \
         {SEARXNG_URL_ENV} to a self-hosted SearXNG instance (with the `json` format \
         enabled). For news, use category \"news\" or a recent `since`. Results-page \
         search (DuckDuckGo, Bing, Brave and Google results pages, the way SearXNG \
         searches) is on unless {SERP_SCRAPE_ENV}=0; an engine that answered with a \
         challenge page is suspended for a while (octos does not solve challenges). Note: \
         search engines' terms may not allow automated queries (Google and Bing: high \
         risk; DuckDuckGo: its robots.txt allows the HTML page, its terms promise \
         nothing).\n"
    )
}
