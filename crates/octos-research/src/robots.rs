//! robots.txt (RFC 9309): parsing, matching and a per-origin cache.
//!
//! Group selection: the group(s) whose `User-agent` equals our product token
//! (case-insensitive), else the `*` group(s). Rule matching: the longest
//! matching path pattern wins; on a tie `Allow` wins. `*` wildcards and a
//! trailing `$` anchor are supported. Fetch outcomes follow the RFC: a 4xx
//! (no robots.txt) allows everything; a 5xx or network failure means
//! "unreachable" and disallows everything for that origin.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use url::Url;

#[derive(Debug, Clone, Default, PartialEq)]
struct Group {
    agents: Vec<String>,
    rules: Vec<(bool, String)>,
    crawl_delay: Option<f64>,
}

/// A parsed robots.txt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Robots {
    groups: Vec<Group>,
}

impl Robots {
    pub fn parse(body: &str) -> Self {
        let mut groups: Vec<Group> = Vec::new();
        let mut current: Option<Group> = None;
        let mut last_was_agent = false;
        for raw in body.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => {
                    if !last_was_agent {
                        if let Some(g) = current.take() {
                            groups.push(g);
                        }
                        current = Some(Group::default());
                    }
                    if let Some(g) = current.as_mut() {
                        g.agents.push(value.to_ascii_lowercase());
                    }
                    last_was_agent = true;
                }
                "allow" | "disallow" => {
                    last_was_agent = false;
                    if let Some(g) = current.as_mut() {
                        if !value.is_empty() {
                            g.rules.push((key == "allow", value.to_string()));
                        }
                    }
                }
                "crawl-delay" => {
                    last_was_agent = false;
                    if let Some(g) = current.as_mut() {
                        g.crawl_delay = value.parse::<f64>().ok().filter(|d| *d >= 0.0);
                    }
                }
                _ => {
                    // Sitemap and unknown keys do not end a group.
                }
            }
        }
        if let Some(g) = current.take() {
            groups.push(g);
        }
        Self { groups }
    }

    fn groups_for(&self, agent: &str) -> Vec<&Group> {
        let agent = agent.to_ascii_lowercase();
        let specific: Vec<&Group> = self
            .groups
            .iter()
            .filter(|g| g.agents.contains(&agent))
            .collect();
        if !specific.is_empty() {
            return specific;
        }
        self.groups
            .iter()
            .filter(|g| g.agents.iter().any(|a| a == "*"))
            .collect()
    }

    /// Whether `agent` may fetch `path` (path plus optional `?query`).
    pub fn is_allowed(&self, agent: &str, path: &str) -> bool {
        if path == "/robots.txt" {
            return true;
        }
        let mut best: Option<(usize, bool)> = None;
        for g in self.groups_for(agent) {
            for (allow, pattern) in &g.rules {
                if let Some(len) = match_len(pattern, path) {
                    best = match best {
                        Some((blen, ballow)) if blen > len || (blen == len && ballow) => {
                            Some((blen, ballow))
                        }
                        _ => Some((len, *allow)),
                    };
                }
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }

    /// `Crawl-delay` for `agent`, if the matching group sets one.
    pub fn crawl_delay(&self, agent: &str) -> Option<Duration> {
        self.groups_for(agent)
            .iter()
            .filter_map(|g| g.crawl_delay)
            .fold(None, |acc: Option<f64>, d| {
                Some(acc.map_or(d, |a| a.max(d)))
            })
            .map(Duration::from_secs_f64)
    }
}

/// Length of `pattern` if it matches `path` (prefix match with `*` and `$`).
fn match_len(pattern: &str, path: &str) -> Option<usize> {
    let (pat, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    if glob_match(pat.as_bytes(), path.as_bytes(), anchored) {
        Some(pattern.len())
    } else {
        None
    }
}

fn glob_match(pat: &[u8], text: &[u8], anchored: bool) -> bool {
    match pat.split_first() {
        None => !anchored || text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|i| glob_match(rest, &text[i..], anchored)),
        Some((c, rest)) => text
            .split_first()
            .is_some_and(|(t, trest)| t == c && glob_match(rest, trest, anchored)),
    }
}

/// Outcome of fetching an origin's robots.txt.
#[derive(Debug, Clone, PartialEq)]
pub enum RobotsStatus {
    /// 4xx: no robots.txt, everything allowed.
    AllowAll,
    /// 5xx or unreachable: assume complete disallow (RFC 9309 §2.3.1.4).
    Unreachable,
    Parsed(Robots),
}

impl RobotsStatus {
    /// Map an HTTP status and body to a status. `None` = network failure.
    pub fn from_response(status: Option<u16>, body: &str) -> Self {
        match status {
            Some(s) if (200..300).contains(&s) => RobotsStatus::Parsed(Robots::parse(body)),
            Some(s) if (400..500).contains(&s) => RobotsStatus::AllowAll,
            _ => RobotsStatus::Unreachable,
        }
    }

    pub fn is_allowed(&self, agent: &str, path: &str) -> bool {
        match self {
            RobotsStatus::AllowAll => true,
            RobotsStatus::Unreachable => false,
            RobotsStatus::Parsed(r) => r.is_allowed(agent, path),
        }
    }

    pub fn crawl_delay(&self, agent: &str) -> Option<Duration> {
        match self {
            RobotsStatus::Parsed(r) => r.crawl_delay(agent),
            _ => None,
        }
    }
}

/// `https://host:port/robots.txt` for a URL, plus the path+query to check.
pub fn robots_location(page_url: &str) -> Option<(String, String)> {
    let u = Url::parse(page_url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let origin = u.origin().ascii_serialization();
    let mut path = u.path().to_string();
    if let Some(q) = u.query() {
        path.push('?');
        path.push_str(q);
    }
    Some((format!("{origin}/robots.txt"), path))
}

type Slot = Arc<tokio::sync::OnceCell<Arc<RobotsStatus>>>;

/// Per-origin robots.txt cache. Concurrent checks for the same origin share
/// one fetch.
#[derive(Default, Clone)]
pub struct RobotsCache {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
}

/// Decision for one URL.
#[derive(Debug, Clone, PartialEq)]
pub struct RobotsDecision {
    pub allowed: bool,
    pub crawl_delay: Option<Duration>,
    /// `unreachable` when robots.txt could not be fetched, else `disallow`.
    pub reason: &'static str,
}

impl RobotsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check `page_url` for `agent`, fetching the origin's robots.txt once
    /// via `fetch(robots_url) -> (status or None on network error, body)`.
    pub async fn check<F, Fut>(&self, page_url: &str, agent: &str, fetch: F) -> RobotsDecision
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = (Option<u16>, String)>,
    {
        let Some((robots_url, path)) = robots_location(page_url) else {
            return RobotsDecision {
                allowed: false,
                crawl_delay: None,
                reason: "invalid_url",
            };
        };
        let slot = {
            let mut map = self.slots.lock().unwrap_or_else(|p| p.into_inner());
            map.entry(robots_url.clone()).or_default().clone()
        };
        let status = slot
            .get_or_init(|| async move {
                let (code, body) = fetch(robots_url).await;
                Arc::new(RobotsStatus::from_response(code, &body))
            })
            .await
            .clone();
        let allowed = status.is_allowed(agent, &path);
        RobotsDecision {
            allowed,
            crawl_delay: status.crawl_delay(agent),
            reason: if allowed {
                "allow"
            } else if *status == RobotsStatus::Unreachable {
                "robots_unreachable"
            } else {
                "robots"
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOGLE_NEWS_LIKE: &str = "User-agent: *\nDisallow: /\nAllow: /$\nAllow: /topics/\n\n\
        User-agent: GPTBot\nUser-agent: ClaudeBot\nDisallow: /\n";

    #[test]
    fn should_disallow_and_allow_by_longest_match() {
        let r = Robots::parse(GOOGLE_NEWS_LIKE);
        assert!(!r.is_allowed("octos-research", "/rss/articles/abc"));
        assert!(r.is_allowed("octos-research", "/"));
        assert!(r.is_allowed("octos-research", "/topics/world"));
        assert!(!r.is_allowed("octos-research", "/home"));
        assert!(r.is_allowed("octos-research", "/robots.txt"));
    }

    #[test]
    fn should_prefer_the_group_naming_our_token() {
        let body = "User-agent: *\nDisallow: /private\n\nUser-agent: octos-research\nDisallow: /\nAllow: /public\nCrawl-delay: 2\n";
        let r = Robots::parse(body);
        assert!(!r.is_allowed("octos-research", "/news"));
        assert!(r.is_allowed("octos-research", "/public/a"));
        assert!(r.is_allowed("someone-else", "/news"));
        assert_eq!(
            r.crawl_delay("octos-research"),
            Some(Duration::from_secs(2))
        );
        assert_eq!(r.crawl_delay("someone-else"), None);
    }

    #[test]
    fn should_support_wildcards_anchor_and_tie_to_allow() {
        let r = Robots::parse(
            "User-agent: *\nDisallow: /*.pdf$\nDisallow: /a\nAllow: /a\nDisallow: /search?q=\n",
        );
        assert!(!r.is_allowed("x", "/docs/file.pdf"));
        assert!(r.is_allowed("x", "/docs/file.pdf?download=1"));
        assert!(r.is_allowed("x", "/a/b"), "tie goes to allow");
        assert!(!r.is_allowed("x", "/search?q=rust"));
        assert!(r.is_allowed("x", "/searching"));
    }

    #[test]
    fn should_allow_everything_for_empty_or_missing_robots() {
        assert!(Robots::parse("").is_allowed("x", "/anything"));
        assert!(Robots::parse("User-agent: *\nDisallow:\n").is_allowed("x", "/anything"));
        assert!(RobotsStatus::from_response(Some(404), "").is_allowed("x", "/a"));
        assert!(!RobotsStatus::from_response(Some(503), "").is_allowed("x", "/a"));
        assert!(!RobotsStatus::from_response(None, "").is_allowed("x", "/a"));
    }

    #[tokio::test]
    async fn should_cache_robots_per_origin_and_fetch_once() {
        let cache = RobotsCache::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for path in ["/private/x", "/public/y", "/private/z"] {
            let calls = calls.clone();
            let d = cache
                .check(
                    &format!("https://example.com{path}"),
                    "octos-research",
                    |u| async move {
                        assert_eq!(u, "https://example.com/robots.txt");
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        (Some(200), "User-agent: *\nDisallow: /private\n".to_string())
                    },
                )
                .await;
            assert_eq!(d.allowed, path.starts_with("/public"), "{path}");
            if !d.allowed {
                assert_eq!(d.reason, "robots");
            }
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let d = cache
            .check("https://down.example/x", "octos-research", |_| async {
                (None, String::new())
            })
            .await;
        assert!(!d.allowed);
        assert_eq!(d.reason, "robots_unreachable");
    }
}
