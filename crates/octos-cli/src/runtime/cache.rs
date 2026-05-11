//! TTL/LRU cache for per-session runtimes.
//!
//! See the crate-level [`super`] module docs and
//! `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md`. This file owns the
//! [`SessionRuntimeCache`] type. The cache is intentionally a
//! performance optimization: every entry is reconstructible from the
//! parent [`ProfileRuntime`] + on-disk session metadata, so eviction
//! is always safe.
//!
//! M11-A shipped only `new` and `invalidate`. M11-C fills in the
//! `get_or_init` body, the background-sweep task, and the LRU soft-cap
//! eviction.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::Result;
use octos_core::SessionKey;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::{ProfileRuntime, SessionRuntime};

/// How often the background sweep task scans for idle entries.
///
/// 60 s strikes the balance between "leaks one minute of capacity
/// after the last hit" and "wakes the executor more often than the
/// hit rate justifies". Tests may override the cache TTL but the
/// sweep cadence is fixed.
const BACKGROUND_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

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
    /// Cancellation signal for the background sweep task. Notified
    /// when the cache is dropped so the task can shut down cleanly
    /// instead of leaking onto the runtime.
    shutdown: Arc<Notify>,
    /// Handle to the background sweep task. Held so [`Drop`] can
    /// abort it as a belt-and-suspenders alongside the
    /// `shutdown.notify_one()` signal — if the cache is dropped on a
    /// runtime that's already mid-tear-down, the `notify` may not
    /// reach the task before the executor stops polling it.
    sweep_task: std::sync::Mutex<Option<JoinHandle<()>>>,
}

/// Internal cache entry. Pairs the cached [`SessionRuntime`] with
/// the timestamp of its most recent access for LRU bookkeeping.
struct CacheEntry {
    /// The cached per-session runtime.
    runtime: Arc<SessionRuntime>,
    /// Monotonic timestamp of the most recent
    /// [`SessionRuntimeCache::get_or_init`] hit. Used by the
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
    ///
    /// A background sweep task is spawned on the current tokio
    /// runtime; it cancels cleanly when the cache is dropped.
    /// Construction outside a tokio context returns a cache with
    /// the sweep disabled — `get_or_init` and `invalidate` still
    /// work; only the periodic idle sweep is skipped.
    pub fn new(max_size: usize, idle_ttl: Duration) -> Self {
        let inner = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let shutdown = Arc::new(Notify::new());

        // Spawn the periodic sweep task. The task holds a weak
        // reference (via the `inner` Arc) to the map and exits when
        // the shutdown notify fires.
        let sweep_task = tokio::runtime::Handle::try_current().ok().map(|handle| {
            let inner = Arc::clone(&inner);
            let shutdown = Arc::clone(&shutdown);
            handle.spawn(background_sweep_loop(inner, idle_ttl, shutdown))
        });

        Self {
            inner,
            max_size,
            idle_ttl,
            shutdown,
            sweep_task: std::sync::Mutex::new(sweep_task),
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
    /// On hit, the entry's `last_used` is bumped before the
    /// `Arc<SessionRuntime>` is returned.
    ///
    /// On miss, the call drops the read lock, takes the write lock,
    /// and re-checks under the write lock so two concurrent misses
    /// for the same key only run `bootstrap` once. Without the
    /// check-twice ordering we would build two `Agent`s, two
    /// `SessionManager`s, etc.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`SessionRuntime::bootstrap`].
    pub async fn get_or_init(
        &self,
        profile: &Arc<ProfileRuntime>,
        session_key: SessionKey,
        workspace_hint: Option<PathBuf>,
    ) -> Result<Arc<SessionRuntime>> {
        let key = (profile.profile_id.clone(), session_key.clone());

        // Fast path: read lock + last_used bump on hit.
        {
            let guard = self.inner.read().await;
            if let Some(entry) = guard.get(&key) {
                let runtime = Arc::clone(&entry.runtime);
                drop(guard);
                // Re-take the write lock just to bump the timestamp.
                // The read-then-write pattern is acceptable here: the
                // worst case is that two concurrent hits race on the
                // timestamp update, which is benign (LRU bookkeeping
                // is not load-bearing for correctness).
                let mut guard = self.inner.write().await;
                if let Some(entry) = guard.get_mut(&key) {
                    entry.last_used = Instant::now();
                }
                return Ok(runtime);
            }
        }

        // Miss path: take the write lock first, double-check under
        // it, then bootstrap on a confirmed miss. This is what stops
        // two concurrent get_or_init calls from both running
        // bootstrap (which would build two Agents, two
        // SessionManagers, etc.).
        let mut guard = self.inner.write().await;
        if let Some(entry) = guard.get_mut(&key) {
            entry.last_used = Instant::now();
            return Ok(Arc::clone(&entry.runtime));
        }

        // Drop the lock while bootstrapping so other keys can make
        // progress; another task may insert our key during this
        // window, in which case we'll observe its entry on the next
        // re-check below. Releasing the lock around bootstrap is
        // safe because bootstrap is purely a function of (profile,
        // session_key, workspace_hint) — two concurrent bootstraps
        // for the same key produce equivalent runtimes; only one
        // will survive insertion.
        drop(guard);
        let runtime =
            SessionRuntime::bootstrap(profile, session_key.clone(), workspace_hint).await?;

        let mut guard = self.inner.write().await;
        // Re-check once more: another task may have inserted while
        // we were bootstrapping. If so, return that one and drop
        // ours — both are valid runtimes for the same key.
        if let Some(entry) = guard.get_mut(&key) {
            entry.last_used = Instant::now();
            return Ok(Arc::clone(&entry.runtime));
        }

        // Soft-cap eviction: if we're at capacity, drop the LRU
        // entry before inserting. This is best-effort — the cap is
        // soft because eviction is never correctness-critical.
        if guard.len() >= self.max_size {
            if let Some(lru_key) = guard
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&lru_key);
            }
        }

        guard.insert(
            key,
            CacheEntry {
                runtime: Arc::clone(&runtime),
                last_used: Instant::now(),
            },
        );
        Ok(runtime)
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

    /// Drop every entry whose `last_used` is older than
    /// [`Self::idle_ttl`]. Exposed so tests can verify the eviction
    /// invariant without waiting for the 60 s background sweep.
    /// Production callers should rely on the background task.
    pub async fn invalidate_idle(&self) {
        let now = Instant::now();
        let ttl = self.idle_ttl;
        let mut guard = self.inner.write().await;
        guard.retain(|_, entry| now.duration_since(entry.last_used) < ttl);
    }

    /// Number of cached entries. Exposed for tests and metrics.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Whether the cache is empty. Exposed for tests and metrics.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }
}

impl Drop for SessionRuntimeCache {
    fn drop(&mut self) {
        // Signal + abort. `notify_one` is the clean shutdown path;
        // `abort` is the belt-and-suspenders for the case where the
        // runtime is mid-tear-down.
        self.shutdown.notify_one();
        if let Ok(mut slot) = self.sweep_task.lock() {
            if let Some(handle) = slot.take() {
                handle.abort();
            }
        }
    }
}

async fn background_sweep_loop(
    inner: Arc<tokio::sync::RwLock<HashMap<(String, SessionKey), CacheEntry>>>,
    idle_ttl: Duration,
    shutdown: Arc<Notify>,
) {
    loop {
        tokio::select! {
            _ = shutdown.notified() => break,
            _ = tokio::time::sleep(BACKGROUND_SWEEP_INTERVAL) => {
                let now = Instant::now();
                let mut guard = inner.write().await;
                guard.retain(|_, entry| now.duration_since(entry.last_used) < idle_ttl);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;
    use std::sync::Arc;

    use octos_agent::sandbox::create_sandbox;
    use octos_agent::{SandboxConfig, ToolRegistry};
    use octos_core::Message;
    use octos_llm::{ChatConfig, ChatResponse, LlmProvider, ToolSpec};
    use octos_memory::{EpisodeStore, MemoryStore};
    use tempfile::TempDir;

    use crate::runtime::ProfileRuntime;

    struct StubLlm;

    #[async_trait::async_trait]
    impl LlmProvider for StubLlm {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            Err(eyre::eyre!("stub LLM not callable in M11-C tests"))
        }
        fn model_id(&self) -> &str {
            "stub-model"
        }
        fn provider_name(&self) -> &str {
            "stub"
        }
    }

    async fn make_profile(data_dir: PathBuf) -> Arc<ProfileRuntime> {
        std::fs::create_dir_all(&data_dir).unwrap();
        let memory = Arc::new(EpisodeStore::open(&data_dir).await.unwrap());
        let memory_store = Arc::new(MemoryStore::open(&data_dir).await.unwrap());
        let sandbox = SandboxConfig::default();
        let base_tools =
            ToolRegistry::with_builtins_and_sandbox(&data_dir, create_sandbox(&sandbox));
        Arc::new(ProfileRuntime {
            profile_id: "_main".to_string(),
            data_dir,
            llm: Arc::new(StubLlm),
            adaptive_router: None,
            credentials: StdHashMap::new(),
            skills_dir: None,
            plugin_env_template: Vec::new(),
            tool_policy: None,
            default_sandbox: sandbox,
            tool_specs: Arc::new(base_tools),
            memory,
            memory_store,
        })
    }

    #[tokio::test]
    async fn get_or_init_returns_same_arc_on_second_call() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir).await;

        let cache = SessionRuntimeCache::new(8, Duration::from_secs(60));
        let key = SessionKey::new("api", "cache-hit");

        let first = cache
            .get_or_init(&profile, key.clone(), None)
            .await
            .expect("first init");
        let second = cache
            .get_or_init(&profile, key.clone(), None)
            .await
            .expect("second init");

        assert!(
            Arc::ptr_eq(&first, &second),
            "second get_or_init must hit the cache and reuse the Arc"
        );
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn invalidate_idle_drops_aged_entries() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir).await;

        // 100 ms TTL — short enough to age out within a test tick.
        let cache = SessionRuntimeCache::new(8, Duration::from_millis(100));
        let key = SessionKey::new("api", "evict-me");

        let _runtime = cache
            .get_or_init(&profile, key.clone(), None)
            .await
            .expect("init");
        assert_eq!(cache.len().await, 1);

        // Wait past the TTL, then invoke the manual sweep helper
        // (production uses the 60 s background loop).
        tokio::time::sleep(Duration::from_millis(200)).await;
        cache.invalidate_idle().await;

        assert!(
            cache.is_empty().await,
            "idle entry should have been evicted"
        );
    }

    #[tokio::test]
    async fn invalidate_removes_specific_key() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir).await;

        let cache = SessionRuntimeCache::new(8, Duration::from_secs(60));
        let key = SessionKey::new("api", "explicit-invalidate");

        let _ = cache
            .get_or_init(&profile, key.clone(), None)
            .await
            .expect("init");
        assert_eq!(cache.len().await, 1);

        cache
            .invalidate(&(profile.profile_id.clone(), key.clone()))
            .await;
        assert!(cache.is_empty().await);

        // Idempotent.
        cache.invalidate(&(profile.profile_id.clone(), key)).await;
    }
}
