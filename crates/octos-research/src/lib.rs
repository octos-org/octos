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
pub mod plan;
pub mod providers;
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

/// Environment variable that opts in to the headless-browser search-results
/// scrape (Bing rendered in Chrome). Off unless set to `1`/`true`/`yes`.
/// ADR 0002 forbids disguised search, so this exists only for operators who
/// explicitly accept that trade-off on their own machine.
pub const BROWSER_SERP_ENV: &str = "OCTOS_ALLOW_BROWSER_SERP";

/// Environment variable naming a self-hosted SearXNG base URL
/// (e.g. `http://127.0.0.1:8888`).
pub const SEARXNG_URL_ENV: &str = "SEARXNG_URL";

/// Whether the browser search-results scrape is explicitly enabled, given an
/// env lookup (injected so tests never touch process env).
pub fn browser_serp_allowed(lookup: impl Fn(&str) -> Option<String>) -> bool {
    lookup(BROWSER_SERP_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_keep_browser_serp_off_when_env_is_unset_or_falsy() {
        assert!(!browser_serp_allowed(|_| None));
        assert!(!browser_serp_allowed(|_| Some("0".into())));
        assert!(!browser_serp_allowed(|_| Some("".into())));
        assert!(browser_serp_allowed(|_| Some("1".into())));
        assert!(browser_serp_allowed(|_| Some("TRUE".into())));
    }

    #[test]
    fn should_identify_itself_in_user_agent() {
        assert!(USER_AGENT.contains(AGENT_TOKEN));
        assert!(!USER_AGENT.contains("Chrome/"));
    }
}
