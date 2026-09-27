//! Per-host politeness: a minimum interval between requests to one host.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

/// Spaces requests to the same host at least `min_interval` apart (or the
/// host's robots.txt `Crawl-delay`, when larger, capped at `max_delay`).
/// Slots are reserved under a lock, so concurrent callers queue up instead
/// of all firing when the interval elapses.
#[derive(Clone)]
pub struct HostThrottle {
    min_interval: Duration,
    max_delay: Duration,
    next_slot: Arc<Mutex<HashMap<String, Instant>>>,
}

impl HostThrottle {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            max_delay: Duration::from_secs(10),
            next_slot: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Reserve the next slot for `host` and return how long to wait for it.
    pub fn reserve(&self, host: &str, crawl_delay: Option<Duration>) -> Duration {
        let interval = crawl_delay
            .map(|d| d.min(self.max_delay))
            .unwrap_or_default()
            .max(self.min_interval);
        let key = host.to_ascii_lowercase();
        let now = Instant::now();
        let mut map = self.next_slot.lock().unwrap_or_else(|p| p.into_inner());
        let slot = map.get(&key).copied().filter(|t| *t > now).unwrap_or(now);
        map.insert(key, slot + interval);
        slot.saturating_duration_since(now)
    }

    /// Push this host's next slot at least `delay` into the future (e.g. a
    /// 429/503 `Retry-After`), so concurrent readers back off too.
    pub fn defer(&self, host: &str, delay: Duration) {
        let key = host.to_ascii_lowercase();
        let until = Instant::now() + delay;
        let mut map = self.next_slot.lock().unwrap_or_else(|p| p.into_inner());
        let slot = map.entry(key).or_insert(until);
        if *slot < until {
            *slot = until;
        }
    }

    /// Wait for this host's next slot.
    pub async fn wait(&self, host: &str, crawl_delay: Option<Duration>) {
        let d = self.reserve(host, crawl_delay);
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_space_requests_to_the_same_host() {
        let t = HostThrottle::new(Duration::from_millis(200));
        assert_eq!(t.reserve("a.com", None), Duration::ZERO);
        let second = t.reserve("a.com", None);
        assert!(second > Duration::from_millis(150), "{second:?}");
        let third = t.reserve("a.com", None);
        assert!(third > Duration::from_millis(350), "{third:?}");
        assert_eq!(
            t.reserve("b.com", None),
            Duration::ZERO,
            "other hosts are independent"
        );
    }

    #[tokio::test]
    async fn should_defer_a_host_after_backoff() {
        let t = HostThrottle::new(Duration::from_millis(10));
        t.defer("a.com", Duration::from_secs(5));
        let wait = t.reserve("a.com", None);
        assert!(wait > Duration::from_millis(4900), "{wait:?}");
        assert_eq!(t.reserve("b.com", None), Duration::ZERO);
    }

    #[tokio::test]
    async fn should_honor_crawl_delay_up_to_the_cap() {
        let t = HostThrottle::new(Duration::from_millis(10));
        t.reserve("a.com", Some(Duration::from_secs(3)));
        let wait = t.reserve("a.com", None);
        assert!(wait > Duration::from_millis(2900), "{wait:?}");
        t.reserve("b.com", Some(Duration::from_secs(3600)));
        let capped = t.reserve("b.com", None);
        assert!(capped <= Duration::from_secs(10), "{capped:?}");
    }
}
