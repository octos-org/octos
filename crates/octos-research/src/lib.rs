//! Research building blocks shared by the octos search tools (the built-in
//! `web_search` / `search` tools and the `deep-search` / `deep-crawl` skills).
//!
//! Policy (OctoSense ADR 0002, section 6): free structured sources first
//! (GDELT, Google News RSS), then a self-hosted SearXNG if one is configured,
//! then search API keys the person chose to add, then the DuckDuckGo and Bing
//! results pages for general web search (on by default; an operator can turn
//! them off). Pages that will be cited are *read* (optionally rendered by a
//! real browser) at a polite rate, with an identifiable User-Agent. Nothing
//! here disguises automation, imitates a person or solves CAPTCHAs.
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

/// Environment variable that opts in to scraping search-engine results
/// pages: the keyless DuckDuckGo HTML endpoint and the Bing results page
/// rendered in headless Chrome. **On by default** (OctoSense ADR 0002 §6:
/// general web search for a personal assistant); set it to
/// `0`/`false`/`no`/`off` to turn results-page search off. It is always
/// honest: identifiable User-Agent, no stealth, no CAPTCHA solving; a
/// challenge page ends that provider's attempt.
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

/// Person's-browser mode for results pages that need a real browser
/// (Google): `auto` (default) | `window` | `headless` | `off`. See
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

/// Environment variable naming a self-hosted SearXNG base URL
/// (e.g. `http://127.0.0.1:8888`).
pub const SEARXNG_URL_ENV: &str = "SEARXNG_URL";

/// Whether results-page search (DuckDuckGo HTML, Bing in a browser) is on.
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
        "Results-page search (DuckDuckGo's HTML page, Bing in headless Chrome) is now on by \
         default for general web results (OctoSense ADR 0002 amendment). Search engines' terms \
         may not allow automated queries (Bing: high risk; DuckDuckGo: its robots.txt allows \
         the HTML page, its terms promise nothing). Set OCTOS_ALLOW_SERP_SCRAPE=0 to turn it off.",
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
         search (DuckDuckGo, Bing; an interim layer until the metasearch's own \
         results-page engines replace it) is on unless {SERP_SCRAPE_ENV}=0; if it was \
         tried, the engines may have answered with a challenge page, which octos does \
         not bypass. Note: search engines' terms may not allow automated queries \
         (Bing: high risk; DuckDuckGo: its robots.txt allows the HTML page, its terms \
         promise nothing).\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_keep_results_page_search_on_unless_turned_off() {
        assert!(serp_scrape_allowed(|_| None), "on by default");
        let only =
            |key: &'static str, v: &'static str| move |k: &str| (k == key).then(|| v.to_string());
        for on in ["1", "TRUE", "yes", "on"] {
            assert!(serp_scrape_allowed(only(SERP_SCRAPE_ENV, on)), "{on}");
        }
        // Set to anything else, including empty or a typo: off (fail safe).
        for off in ["0", "false", "NO", "off", "", "  ", "disabled", "nope"] {
            assert!(!serp_scrape_allowed(only(SERP_SCRAPE_ENV, off)), "{off:?}");
        }
        assert!(!serp_scrape_allowed(only(BROWSER_SERP_ENV, "0")), "alias");
        assert!(serp_scrape_allowed(only("OTHER", "0")));
    }

    #[test]
    fn should_give_the_default_notice_only_when_unset() {
        assert!(serp_scrape_default_notice(|_| None).is_some_and(|n| n.contains("=0")));
        assert!(
            serp_scrape_default_notice(|k| (k == SERP_SCRAPE_ENV).then(|| "1".into())).is_none()
        );
        assert!(
            serp_scrape_default_notice(|k| (k == BROWSER_SERP_ENV).then(|| "0".into())).is_none()
        );
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
    fn should_say_what_browser_search_means_and_how_to_stop_it() {
        let n = BROWSER_SEARCH_NOTICE;
        assert!(n.contains("Google account") && n.contains("search activity"));
        assert!(n.contains("terms may not allow"));
        assert!(n.contains(&format!("{BROWSER_ENV}=off")));
    }

    #[test]
    fn should_keep_robots_checks_off_by_default() {
        assert!(!respect_robots(|_| None));
        assert!(!respect_robots(|_| Some("0".into())));
        assert!(respect_robots(
            |k| (k == RESPECT_ROBOTS_ENV).then(|| "1".to_string())
        ));
    }

    #[test]
    fn should_identify_itself_in_user_agent() {
        assert!(USER_AGENT.starts_with(AGENT_TOKEN));
        assert!(USER_AGENT.contains("https://github.com/octos-org/octos"));
        assert!(!USER_AGENT.contains("Mozilla") && !USER_AGENT.contains("Chrome/"));
    }
}
