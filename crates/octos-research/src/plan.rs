//! Provider order. Free structured sources first, then a configured
//! SearXNG, then search APIs the person added keys for, then the keyless
//! HTML fallback. The headless-browser search-results scrape is never part
//! of the automatic order; it is appended only when explicitly enabled.

use serde::{Deserialize, Serialize};

use crate::date::Since;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Gdelt,
    GoogleNewsRss,
    Searxng,
    Serper,
    Tavily,
    Exa,
    Perplexity,
    Brave,
    You,
    #[serde(rename = "duckduckgo")]
    DuckDuckGo,
    /// Bing rendered in headless Chrome. Opt-in only (see
    /// [`crate::BROWSER_SERP_ENV`]).
    #[serde(rename = "bing_cdp")]
    BingBrowser,
}

impl Provider {
    pub fn id(self) -> &'static str {
        match self {
            Provider::Gdelt => "gdelt",
            Provider::GoogleNewsRss => "google_news_rss",
            Provider::Searxng => "searxng",
            Provider::Serper => "serper",
            Provider::Tavily => "tavily",
            Provider::Exa => "exa",
            Provider::Perplexity => "perplexity",
            Provider::Brave => "brave",
            Provider::You => "you",
            Provider::DuckDuckGo => "duckduckgo",
            Provider::BingBrowser => "bing_cdp",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id.trim().to_ascii_lowercase().as_str() {
            "gdelt" => Provider::Gdelt,
            "google_news_rss" | "google_news" | "gnews" => Provider::GoogleNewsRss,
            "searxng" => Provider::Searxng,
            "serper" => Provider::Serper,
            "tavily" => Provider::Tavily,
            "exa" => Provider::Exa,
            "perplexity" => Provider::Perplexity,
            "brave" => Provider::Brave,
            "you" | "you.com" => Provider::You,
            "duckduckgo" | "ddg" => Provider::DuckDuckGo,
            "bing_cdp" | "bing" => Provider::BingBrowser,
            _ => return None,
        })
    }

    /// Free, key-less structured sources.
    pub fn is_free_structured(self) -> bool {
        matches!(self, Provider::Gdelt | Provider::GoogleNewsRss)
    }
}

/// `category` control.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// News if the query or `since` looks news-ish, else general.
    #[default]
    Auto,
    News,
    General,
}

impl Category {
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(|s| s.trim().to_ascii_lowercase()) {
            None => Ok(Category::Auto),
            Some(s) if s.is_empty() || s == "auto" => Ok(Category::Auto),
            Some(s) if s == "news" => Ok(Category::News),
            Some(s) if s == "general" || s == "web" => Ok(Category::General),
            Some(s) => Err(format!(
                "invalid category {s:?} (use news, general or auto)"
            )),
        }
    }

    /// Resolve `Auto` for a query.
    pub fn is_news(
        self,
        query: &str,
        since: Option<&Since>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        match self {
            Category::News => true,
            Category::General => false,
            Category::Auto => looks_newsish(query, since, now),
        }
    }
}

/// Heuristic for "news-ish": a recent `since` window (≤ 31 days) or
/// recency/news words in the query (several languages).
pub fn looks_newsish(
    query: &str,
    since: Option<&Since>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if since.is_some_and(|s| s.effective_span(now) <= chrono::Duration::days(31)) {
        return true;
    }
    let q = query.to_lowercase();
    const WORDS: &[&str] = &[
        "news",
        "latest",
        "today",
        "yesterday",
        "this week",
        "breaking",
        "headline",
        "announce",
        "election",
        "新闻",
        "最新",
        "今天",
        "今日",
        "本周",
        "快讯",
        "消息",
        "ニュース",
        "뉴스",
        "noticias",
        "actualités",
        "nachrichten",
        "notizie",
        "новости",
    ];
    WORDS.iter().any(|w| q.contains(w))
}

/// Inputs to [`plan`].
#[derive(Debug, Clone, Default)]
pub struct PlanInput {
    pub news: bool,
    pub searxng_configured: bool,
    /// Keyed providers that have a key, in the caller's priority order.
    pub keyed: Vec<Provider>,
    /// Include the keyless DuckDuckGo HTML endpoint as the last resort.
    pub keyless_fallback: bool,
    /// Operator opt-in for the headless-browser search-results scrape.
    pub allow_browser_serp: bool,
}

/// Automatic provider order.
///
/// news → `[gdelt, google_news_rss]`, then `searxng` if configured, then the
/// keyed providers, then `duckduckgo` (if enabled), and `bing_cdp` only when
/// explicitly allowed.
pub fn plan(input: &PlanInput) -> Vec<Provider> {
    let mut out = Vec::new();
    if input.news {
        out.push(Provider::Gdelt);
        out.push(Provider::GoogleNewsRss);
    }
    if input.searxng_configured {
        out.push(Provider::Searxng);
    }
    for p in &input.keyed {
        if !out.contains(p) && !matches!(p, Provider::BingBrowser | Provider::DuckDuckGo) {
            out.push(*p);
        }
    }
    if input.keyless_fallback {
        out.push(Provider::DuckDuckGo);
    }
    if input.allow_browser_serp {
        out.push(Provider::BingBrowser);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap()
    }

    #[test]
    fn should_put_free_news_sources_first_then_searxng_then_keys() {
        let order = plan(&PlanInput {
            news: true,
            searxng_configured: true,
            keyed: vec![Provider::Serper, Provider::Brave],
            keyless_fallback: true,
            allow_browser_serp: false,
        });
        assert_eq!(
            order,
            vec![
                Provider::Gdelt,
                Provider::GoogleNewsRss,
                Provider::Searxng,
                Provider::Serper,
                Provider::Brave,
                Provider::DuckDuckGo,
            ]
        );
    }

    #[test]
    fn should_never_plan_browser_serp_by_default() {
        for news in [true, false] {
            let order = plan(&PlanInput {
                news,
                searxng_configured: false,
                // Even if a caller lists it among its providers.
                keyed: vec![Provider::BingBrowser],
                keyless_fallback: true,
                allow_browser_serp: false,
            });
            assert!(!order.contains(&Provider::BingBrowser), "{order:?}");
        }
        let opted_in = plan(&PlanInput {
            allow_browser_serp: true,
            keyless_fallback: true,
            ..Default::default()
        });
        assert_eq!(opted_in.last(), Some(&Provider::BingBrowser));
    }

    #[test]
    fn should_skip_news_sources_for_general_queries_without_searxng() {
        let order = plan(&PlanInput {
            news: false,
            keyless_fallback: true,
            ..Default::default()
        });
        assert_eq!(order, vec![Provider::DuckDuckGo]);
    }

    #[test]
    fn should_detect_newsish_queries() {
        assert!(looks_newsish("latest AI regulation news", None, now()));
        assert!(looks_newsish("伊朗 最新 消息", None, now()));
        assert!(!looks_newsish("rust borrow checker tutorial", None, now()));
        let week = Since::parse("7d", now()).unwrap();
        assert!(looks_newsish("rust borrow checker", Some(&week), now()));
        let year = Since::parse("1y", now()).unwrap();
        assert!(!looks_newsish("rust borrow checker", Some(&year), now()));
        assert!(Category::News.is_news("anything", None, now()));
        assert!(!Category::General.is_news("breaking news", None, now()));
    }

    #[test]
    fn should_round_trip_provider_ids() {
        for p in [
            Provider::Gdelt,
            Provider::GoogleNewsRss,
            Provider::Searxng,
            Provider::DuckDuckGo,
            Provider::BingBrowser,
        ] {
            assert_eq!(Provider::from_id(p.id()), Some(p));
        }
        assert_eq!(Category::parse(Some("NEWS")).unwrap(), Category::News);
        assert!(Category::parse(Some("sports")).is_err());
    }
}
