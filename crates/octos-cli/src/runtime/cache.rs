//! TTL/LRU cache for per-session runtimes.
//!
//! See the crate-level [`super`] module docs and
//! `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md`. This file owns the
//! [`SessionRuntimeCache`] type. The cache is intentionally a
//! performance optimization: every entry is reconstructible from the
//! parent [`ProfileRuntime`] + on-disk session metadata, so eviction
//! is always safe.
//!
//! M11-A ships only `new` and `invalidate` (trivial). The
//! `get_or_init` body lands in M11-C.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::Result;
use octos_core::SessionKey;

use super::{ProfileRuntime, SessionRuntime};

/// In-memory cache mapping `(profile_id, session_key)` to an
/// `Arc<SessionRuntime>`.
///
/// # Eviction policy
///
/// - **`max_size`** — a soft cap on the number of cached entries.
///   When the cache exceeds this size, the implementation evicts the
///   least-recently-used entry.
/// - **`idle_ttl`** — entries whose `last_used` is older than this
///   are eligible for background eviction. The exact eviction trigger
///   (lazy on `get_or_init`, periodic sweep, or both) is an M11-C
///   implementation choice; the contract here is only that entries
///   older than `idle_ttl` may disappear without notice.
///
/// Because every [`SessionRuntime`] is reconstructible from disk,
/// eviction is always safe: a subsequent
/// [`Self::get_or_init`] call rebuilds the runtime from the parent
/// [`ProfileRuntime`] + the on-disk session metadata. Callers must
/// not rely on cache residency for correctness.
///
/// # Concurrency
///
/// The cache wraps the inner map in a [`tokio::sync::RwLock`] so
/// multiple readers can fetch concurrently while a single writer
/// inserts. The lock is async because [`Self::get_or_init`] may need
/// to await [`SessionRuntime::bootstrap`] under contention; using
/// the async lock keeps the runtime futures `Send`.
pub struct SessionRuntimeCache {
    inner: Arc<tokio::sync::RwLock<HashMap<(String, SessionKey), CacheEntry>>>,
    max_size: usize,
    idle_ttl: Duration,
}

/// Internal cache entry. Pairs the cached [`SessionRuntime`] with
/// the timestamp of its most recent access for LRU bookkeeping.
///
/// The fields are read by M11-C's `get_or_init` body; in the M11-A
/// skeleton they're write-only, so we silence the dead-code lint
/// rather than ship a no-op accessor.
#[allow(dead_code)]
struct CacheEntry {
    /// The cached per-session runtime.
    runtime: Arc<SessionRuntime>,
    /// Monotonic timestamp of the most recent
    /// [`SessionRuntimeCache::get_or_init`] hit. Used by M11-C's
    /// eviction logic to identify idle entries.
    last_used: Instant,
}

impl SessionRuntimeCache {
    /// Construct an empty cache with the given LRU capacity and
    /// idle TTL.
    ///
    /// `max_size` is the soft cap on cached entries (LRU eviction
    /// kicks in past this). `idle_ttl` is how long an entry may
    /// sit unused before becoming eligible for eviction.
    pub fn new(max_size: usize, idle_ttl: Duration) -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            max_size,
            idle_ttl,
        }
    }

    /// The LRU capacity this cache was constructed with. Exposed
    /// primarily so tests and metrics endpoints can introspect the
    /// configured limit.
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// The idle TTL this cache was constructed with. Exposed for
    /// the same reasons as [`Self::max_size`].
    pub fn idle_ttl(&self) -> Duration {
        self.idle_ttl
    }

    /// Look up a [`SessionRuntime`] by `(profile_id, session_key)`;
    /// construct one via [`SessionRuntime::bootstrap`] on miss.
    ///
    /// # Contract (filled in by M11-C)
    ///
    /// 1. Read-lock the inner map and look for a live entry under
    ///    `(profile.profile_id.clone(), session_key.clone())`.
    /// 2. On hit: update `last_used` and return the cached
    ///    `Arc<SessionRuntime>`.
    /// 3. On miss:
    ///    a. Drop the read lock.
    ///    b. Take the **write** lock first.
    ///    c. Re-check the map under the write lock — if another
    ///       task inserted the entry while we were upgrading,
    ///       return that entry.
    ///    d. Only then call
    ///       [`SessionRuntime::bootstrap(profile, session_key, workspace_hint)`](SessionRuntime::bootstrap),
    ///       insert, and return. The check-twice ordering is
    ///       load-bearing: it's what stops two concurrent misses
    ///       from both running `bootstrap` (which would build two
    ///       agents, two session managers, etc.).
    /// 4. If the post-insert map size exceeds `max_size`, evict
    ///    the least-recently-used entry.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`SessionRuntime::bootstrap`].
    #[allow(unused_variables)]
    pub async fn get_or_init(
        &self,
        profile: &Arc<ProfileRuntime>,
        session_key: SessionKey,
        workspace_hint: Option<PathBuf>,
    ) -> Result<Arc<SessionRuntime>> {
        todo!("M11-C implements this")
    }

    /// Drop the entry for `key` if present. Used by M11-D's
    /// `/api/sessions/:id/delete` handler and by the config
    /// watcher when a profile reload invalidates every cached
    /// session for the profile.
    ///
    /// Idempotent: removing an absent key is a no-op.
    pub async fn invalidate(&self, key: &(String, SessionKey)) {
        let mut guard = self.inner.write().await;
        guard.remove(key);
    }
}
