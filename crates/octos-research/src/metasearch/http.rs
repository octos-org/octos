//! The metasearch's HTTP side: the fetcher callers supply, per-host
//! politeness (minimum spacing, `Retry-After`), and a small conditional
//! cache (ETag / Last-Modified).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::time::Instant;

/// A request an engine described, after the host validated it.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout: Duration,
}

/// A response as the fetcher received it. Header names are lowercase.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<HttpResponse, String>> + Send + 'a>>;

/// Performs one HTTP request. The metasearch has no HTTP client of its own:
/// callers plug in theirs (see `ReqwestFetch` with the `http` feature), and
/// tests plug in recorded responses.
pub trait Fetch: Send + Sync {
    fn fetch(&self, req: HttpRequest) -> FetchFuture<'_>;
}

/// Parse `Retry-After`: delta-seconds or an HTTP-date. Capped at one hour.
pub fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let v = value.trim();
    let cap = Duration::from_secs(3600);
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs).min(cap));
    }
    let when = DateTime::parse_from_rfc2822(v).ok()?.with_timezone(&Utc);
    let delta = (when - now).to_std().unwrap_or(Duration::ZERO);
    Some(delta.min(cap))
}

/// Per-host spacing plus host-wide blocks from `Retry-After` / provider
/// `backoff` hints. Slots are reserved under a lock so concurrent engines
/// queue instead of firing together.
#[derive(Clone, Default)]
pub struct HostGate {
    next_slot: Arc<Mutex<HashMap<String, Instant>>>,
}

impl HostGate {
    /// Reserve the next slot for `host` at least `interval` after the last
    /// one; returns how long to wait.
    pub fn reserve(&self, host: &str, interval: Duration) -> Duration {
        let now = Instant::now();
        let mut map = self.next_slot.lock().unwrap_or_else(|p| p.into_inner());
        let slot = map.get(host).copied().filter(|t| *t > now).unwrap_or(now);
        map.insert(host.to_string(), slot + interval);
        slot.saturating_duration_since(now)
    }

    /// Nothing goes to `host` for `pause` (a 429/503 `Retry-After`).
    pub fn block(&self, host: &str, pause: Duration) {
        let until = Instant::now() + pause;
        let mut map = self.next_slot.lock().unwrap_or_else(|p| p.into_inner());
        let slot = map.entry(host.to_string()).or_insert(until);
        if *slot < until {
            *slot = until;
        }
    }

    /// Time until `host` may be called again, without reserving.
    pub fn wait_hint(&self, host: &str) -> Duration {
        let now = Instant::now();
        let map = self.next_slot.lock().unwrap_or_else(|p| p.into_inner());
        map.get(host)
            .map(|t| t.saturating_duration_since(now))
            .unwrap_or_default()
    }
}

#[derive(Clone)]
struct CacheEntry {
    stored: Instant,
    ttl: Duration,
    etag: Option<String>,
    last_modified: Option<String>,
    response: HttpResponse,
}

/// What the cache says about a request.
pub enum CacheLookup {
    /// Fresh: use it without a request.
    Fresh(HttpResponse),
    /// Stale but revalidatable: send these conditional headers; a 304 means
    /// the stored response is still good.
    Revalidate(Vec<(String, String)>),
    Miss,
}

/// Bounded in-memory cache of successful provider responses, keyed by the
/// request URL as the engine built it (before any key is attached).
#[derive(Clone)]
pub struct ResponseCache {
    entries: Arc<Mutex<HashMap<String, CacheEntry>>>,
    max_entries: usize,
    max_body_bytes: usize,
}

impl Default for ResponseCache {
    fn default() -> Self {
        Self::new(256, 2 * 1024 * 1024)
    }
}

impl ResponseCache {
    pub fn new(max_entries: usize, max_body_bytes: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            max_entries,
            max_body_bytes,
        }
    }

    pub fn lookup(&self, key: &str) -> CacheLookup {
        let map = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let Some(e) = map.get(key) else {
            return CacheLookup::Miss;
        };
        if e.stored.elapsed() < e.ttl {
            return CacheLookup::Fresh(e.response.clone());
        }
        let mut headers = Vec::new();
        if let Some(t) = &e.etag {
            headers.push(("if-none-match".to_string(), t.clone()));
        }
        if let Some(t) = &e.last_modified {
            headers.push(("if-modified-since".to_string(), t.clone()));
        }
        if headers.is_empty() {
            CacheLookup::Miss
        } else {
            CacheLookup::Revalidate(headers)
        }
    }

    /// Stored response for `key` (after a 304), refreshed.
    pub fn revalidated(&self, key: &str) -> Option<HttpResponse> {
        let mut map = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let e = map.get_mut(key)?;
        e.stored = Instant::now();
        Some(e.response.clone())
    }

    pub fn store(&self, key: &str, response: &HttpResponse, ttl: Duration) {
        if response.status != 200 || response.body.len() > self.max_body_bytes {
            return;
        }
        let no_store = response
            .header("cache-control")
            .is_some_and(|v| v.to_ascii_lowercase().contains("no-store"));
        if no_store {
            return;
        }
        let mut map = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() >= self.max_entries && !map.contains_key(key) {
            // Evict the oldest entry.
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.stored)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(
            key.to_string(),
            CacheEntry {
                stored: Instant::now(),
                ttl,
                etag: response.header("etag").map(String::from),
                last_modified: response.header("last-modified").map(String::from),
                response: response.clone(),
            },
        );
    }
}

/// A [`Fetch`] over reqwest with the identifiable octos User-Agent.
#[cfg(feature = "http")]
#[derive(Clone)]
pub struct ReqwestFetch {
    client: reqwest::Client,
    max_body_bytes: usize,
}

#[cfg(feature = "http")]
impl ReqwestFetch {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            // Provider APIs answer directly; a redirect to another host would
            // leave the engine's declared host list.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            max_body_bytes: 4 * 1024 * 1024,
        }
    }
}

#[cfg(feature = "http")]
impl Default for ReqwestFetch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "http")]
impl Fetch for ReqwestFetch {
    fn fetch(&self, req: HttpRequest) -> FetchFuture<'_> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(req.method.as_bytes())
                .map_err(|e| format!("bad method: {e}"))?;
            let mut rb = self.client.request(method, &req.url).timeout(req.timeout);
            for (k, v) in &req.headers {
                rb = rb.header(k, v);
            }
            if let Some(body) = req.body {
                rb = rb.body(body);
            }
            let mut resp = rb.send().await.map_err(|e| e.without_url().to_string())?;
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    Some((
                        k.as_str().to_ascii_lowercase(),
                        v.to_str().ok()?.to_string(),
                    ))
                })
                .collect();
            let mut bytes = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| e.without_url().to_string())?
            {
                if bytes.len() + chunk.len() > self.max_body_bytes {
                    return Err(format!("response over {} bytes", self.max_body_bytes));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(HttpResponse {
                status,
                headers,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn should_parse_retry_after_seconds_and_http_date() {
        let now = Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap();
        assert_eq!(parse_retry_after("30", now), Some(Duration::from_secs(30)));
        assert_eq!(
            parse_retry_after("Sun, 27 Sep 2026 12:02:00 GMT", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Sun, 27 Sep 2026 11:00:00 GMT", now),
            Some(Duration::ZERO),
            "a date in the past means now"
        );
        assert_eq!(
            parse_retry_after("999999", now),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[tokio::test]
    async fn should_space_hosts_and_honour_blocks() {
        let g = HostGate::default();
        let i = Duration::from_millis(500);
        assert_eq!(g.reserve("a.org", i), Duration::ZERO);
        assert!(g.reserve("a.org", i) > Duration::from_millis(400));
        assert_eq!(
            g.reserve("b.org", i),
            Duration::ZERO,
            "hosts are independent"
        );
        g.block("c.org", Duration::from_secs(30));
        assert!(g.reserve("c.org", i) > Duration::from_secs(29));
        assert!(g.wait_hint("c.org") > Duration::from_secs(29));
    }

    #[tokio::test]
    async fn should_revalidate_stale_entries_with_validators() {
        let c = ResponseCache::new(2, 1024);
        let resp = HttpResponse {
            status: 200,
            headers: vec![("etag".into(), "\"v1\"".into())],
            body: "{}".into(),
        };
        c.store("k", &resp, Duration::from_secs(60));
        assert!(matches!(c.lookup("k"), CacheLookup::Fresh(_)));
        c.store("s", &resp, Duration::ZERO);
        match c.lookup("s") {
            CacheLookup::Revalidate(h) => {
                assert_eq!(h, vec![("if-none-match".into(), "\"v1\"".into())])
            }
            _ => panic!("expected revalidate"),
        }
        assert_eq!(c.revalidated("s").unwrap().body, "{}");
        // Errors and no-store responses are never cached.
        c.store(
            "e",
            &HttpResponse {
                status: 500,
                ..Default::default()
            },
            Duration::from_secs(60),
        );
        assert!(matches!(c.lookup("e"), CacheLookup::Miss));
    }
}
