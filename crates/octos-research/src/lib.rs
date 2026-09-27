//! Research building blocks shared by the octos search tools (the built-in
//! `web_search` / `search` tools and the `deep-search` / `deep-crawl` skills).
//!
//! Policy (OctoSense ADR 0002, section 6): free structured sources first
//! (GDELT, Google News RSS), then a self-hosted SearXNG if one is configured,
//! then search API keys the person chose to add. Pages that will be cited are
//! *read* (optionally rendered by a real browser) at a polite rate that
//! respects robots.txt, with an identifiable User-Agent. Nothing here disguises
//! automation or scrapes a search-results page.
//!
//! This crate is deliberately network-free: it builds provider request URLs,
//! parses provider responses, filters and caps results, parses robots.txt and
//! extracts main text + metadata from HTML. Callers own the HTTP client (each
//! has its own SSRF-safe fetch path), which keeps every piece here testable
//! from fixtures.

pub mod date;
#[cfg(feature = "extract")]
pub mod extract;
pub mod filter;
pub mod item;
pub mod lang;
#[cfg(feature = "fetch")]
pub mod net;
pub mod plan;
pub mod providers;
#[cfg(all(feature = "fetch", feature = "extract"))]
pub mod reader;
pub mod robots;
pub mod throttle;
pub mod urls;

pub use filter::{DomainCap, Filters, OneOrMany};
pub use item::{ItemsDocument, ResearchItem, SearchHit, SkippedUrl, SummaryKind};
pub use plan::{Category, Provider};
pub use robots::{Robots, RobotsCache, RobotsStatus};
pub use throttle::HostThrottle;

/// robots.txt product token matched against `User-agent:` lines.
pub const AGENT_TOKEN: &str = "octos-research";

/// Identifiable User-Agent for every request the research tools make on
/// their own behalf (provider APIs, robots.txt, page reads). It names the
/// software and where to learn about it, instead of posing as a desktop
/// browser.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (compatible; octos-research/1.0; +https://github.com/octos-org/octos)";

/// Environment variable that opts in to scraping search-engine results
/// pages: the keyless DuckDuckGo HTML endpoint and the Bing results page
/// rendered in headless Chrome. Off unless set to `1`/`true`/`yes`.
/// ADR 0002 rules out scraping search results pages, so this exists only for
/// operators who explicitly accept that trade-off on their own machine.
pub const SERP_SCRAPE_ENV: &str = "OCTOS_ALLOW_SERP_SCRAPE";

/// Earlier name of [`SERP_SCRAPE_ENV`], still honoured as an alias.
pub const BROWSER_SERP_ENV: &str = "OCTOS_ALLOW_BROWSER_SERP";

/// Environment variable naming a self-hosted SearXNG base URL
/// (e.g. `http://127.0.0.1:8888`).
pub const SEARXNG_URL_ENV: &str = "SEARXNG_URL";

/// Whether search-results-page scraping (DuckDuckGo HTML, Bing in a
/// browser) is explicitly enabled, via [`SERP_SCRAPE_ENV`] or its alias
/// [`BROWSER_SERP_ENV`]. The env lookup is injected so tests never touch
/// process env.
pub fn serp_scrape_allowed(lookup: impl Fn(&str) -> Option<String>) -> bool {
    [SERP_SCRAPE_ENV, BROWSER_SERP_ENV].iter().any(|k| {
        lookup(k)
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    })
}

/// Message for a search where no allowed provider returned anything: what
/// was tried and how to get results without scraping search pages.
pub fn no_results_message(query: &str, tried: &[String]) -> String {
    let tried = if tried.is_empty() {
        "none (the query is not news-ish and no SearXNG or search API key is configured)"
            .to_string()
    } else {
        tried.join(", ")
    };
    format!(
        "No results for: {query}\n\nProviders tried: {tried}.\n\n\
         Search-results pages are not scraped by default (OctoSense ADR 0002). To get \
         results for general queries, either set {SEARXNG_URL_ENV} to a self-hosted \
         SearXNG instance (with the `json` format enabled), or add a search API key \
         (SERPER_API_KEY, TAVILY_API_KEY, BRAVE_API_KEY, YDC_API_KEY or \
         PERPLEXITY_API_KEY). For news, use category \"news\" or a recent `since` so \
         GDELT and Google News are used. An operator can opt in to scraping \
         DuckDuckGo/Bing results pages with {SERP_SCRAPE_ENV}=1 (not recommended).\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_keep_serp_scraping_off_unless_opted_in() {
        assert!(!serp_scrape_allowed(|_| None));
        assert!(!serp_scrape_allowed(|_| Some("0".into())));
        assert!(!serp_scrape_allowed(|_| Some("".into())));
        let only =
            |key: &'static str, v: &'static str| move |k: &str| (k == key).then(|| v.to_string());
        assert!(serp_scrape_allowed(only(SERP_SCRAPE_ENV, "1")));
        assert!(serp_scrape_allowed(only(SERP_SCRAPE_ENV, "TRUE")));
        assert!(serp_scrape_allowed(only(BROWSER_SERP_ENV, "1")), "alias");
        assert!(!serp_scrape_allowed(only("OTHER", "1")));
    }

    #[test]
    fn should_explain_how_to_get_results_without_scraping() {
        let m = no_results_message("q", &["gdelt".into(), "google_news_rss".into()]);
        assert!(m.contains("Providers tried: gdelt, google_news_rss"));
        assert!(m.contains(SEARXNG_URL_ENV) && m.contains("TAVILY_API_KEY"));
        assert!(m.contains(SERP_SCRAPE_ENV));
        assert!(no_results_message("q", &[]).contains("Providers tried: none"));
    }

    #[test]
    fn should_identify_itself_in_user_agent() {
        assert!(USER_AGENT.contains(AGENT_TOKEN));
        assert!(!USER_AGENT.contains("Chrome/"));
    }
}
