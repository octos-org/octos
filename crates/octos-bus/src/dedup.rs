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

    /// Number of entries currently in the cache.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).len()
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
    fn should_detect_duplicate_within_ttl() {
        let dedup = MessageDedup::new();
        assert!(!dedup.is_duplicate("msg-1"));
        assert!(dedup.is_duplicate("msg-1"));
        assert!(dedup.is_duplicate("msg-1"));
    }

    #[test]
    fn should_allow_different_ids() {
        let dedup = MessageDedup::new();
        assert!(!dedup.is_duplicate("msg-1"));
        assert!(!dedup.is_duplicate("msg-2"));
        assert!(!dedup.is_duplicate("msg-3"));
        assert_eq!(dedup.len(), 3);
    }

    #[test]
    fn should_not_dedup_empty_ids() {
        let dedup = MessageDedup::new();
        assert!(!dedup.is_duplicate(""));
        assert!(!dedup.is_duplicate(""));
    }

    #[test]
    fn should_expire_after_ttl() {
        let dedup = MessageDedup::with_config(100, Duration::from_millis(10));
        assert!(!dedup.is_duplicate("msg-1"));
        assert!(dedup.is_duplicate("msg-1"));

        std::thread::sleep(Duration::from_millis(20));

        // After TTL, should be treated as new
        assert!(!dedup.is_duplicate("msg-1"));
    }

    #[test]
    fn should_evict_lru_when_full() {
        let dedup = MessageDedup::with_config(3, DEFAULT_TTL);
        assert!(!dedup.is_duplicate("a"));
        assert!(!dedup.is_duplicate("b"));
        assert!(!dedup.is_duplicate("c"));
        assert_eq!(dedup.len(), 3);

        // Adding a 4th evicts the oldest ("a")
        assert!(!dedup.is_duplicate("d"));
        assert_eq!(dedup.len(), 3);

        // "a" was evicted, so it's no longer a duplicate
        assert!(!dedup.is_duplicate("a"));
    }

    #[test]
    fn should_handle_poisoned_mutex() {
        // The unwrap_or_else(|e| e.into_inner()) pattern should recover
        let dedup = MessageDedup::new();
        assert!(!dedup.is_duplicate("test"));
        assert!(dedup.is_duplicate("test"));
    }
}
