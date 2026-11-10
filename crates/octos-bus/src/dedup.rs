//! Message deduplication via LRU cache with TTL.
//!
//! Webhook platforms (Feishu, Twilio, WeCom) can deliver the same message
//! multiple times on timeout/retry. This module provides a shared, TTL-aware
//! dedup cache that any channel or the gateway dispatcher can use.

use std::time::{Duration, Instant};

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Mutex;

/// Default capacity: 1000 message IDs.
const DEFAULT_CAPACITY: usize = 1000;

/// Default TTL: 60 seconds.
const DEFAULT_TTL: Duration = Duration::from_secs(60);

/// Thread-safe message deduplication cache with LRU eviction and TTL expiry.
pub struct MessageDedup {
    seen: Mutex<LruCache<String, Instant>>,
    ttl: Duration,
}

impl MessageDedup {
    /// Create a new dedup cache with default capacity (1000) and TTL (60s).
    pub fn new() -> Self {
        Self::with_config(DEFAULT_CAPACITY, DEFAULT_TTL)
    }

    /// Create a dedup cache with custom capacity and TTL.
    pub fn with_config(capacity: usize, ttl: Duration) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(1).unwrap());
        Self {
            seen: Mutex::new(LruCache::new(cap)),
            ttl,
        }
    }

    /// Check if a message ID is a duplicate.
    ///
    /// Returns `true` if the ID was seen within the TTL window (duplicate).
    /// Returns `false` and records the ID if it's new.
    pub fn is_duplicate(&self, id: &str) -> bool {
        if id.is_empty() {
            return false; // Empty IDs are never deduped
        }

        let mut cache = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        if let Some(first_seen) = cache.get(id) {
            if now.duration_since(*first_seen) < self.ttl {
                return true; // Seen recently — duplicate
            }
            // Expired — treat as new, update timestamp
            cache.put(id.to_string(), now);
            false
        } else {
            cache.put(id.to_string(), now);
            false
        }
    }

    /// Remove an ID from the cache so a later delivery is treated as new.
    ///
    /// [`is_duplicate`](Self::is_duplicate) records an ID at check time. If
    /// processing then fails and the platform will retry the same ID (e.g. a
    /// Matrix transaction nacked with a non-2xx status), the entry must be
    /// forgotten or the retry would be dropped as a duplicate.
    pub fn forget(&self, id: &str) {
        let mut cache = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        cache.pop(id);
    }
}

impl Default for MessageDedup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_id_again_after_forget() {
        let dedup = MessageDedup::new();
        assert!(!dedup.is_duplicate("msg-1"));
        dedup.forget("msg-1");
        // Forgotten — treated as new again, then deduped as usual.
        assert!(!dedup.is_duplicate("msg-1"));
        assert!(dedup.is_duplicate("msg-1"));
    }
}
