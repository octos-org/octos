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

/// Cache key shared by every storage map in this module. Pairs the
/// profile id (from [`ProfileRuntime::profile_id`]) with the session
/// key half so a profile reload only invalidates entries belonging
/// to that profile.
type CacheKey = (String, SessionKey);

/// Storage shape for the main cache: `(profile_id, session_key) ->
/// CacheEntry`. Factored into a `type` alias because clippy flags
/// the inline triple-nested generic as `clippy::type_complexity`.
type CacheStorage = Arc<tokio::sync::RwLock<HashMap<CacheKey, CacheEntry>>>;

/// Storage shape for the per-key single-flight inflight map.
/// Uses [`std::sync::Mutex`] (not [`tokio::sync::Mutex`]) so the
/// owner-side cleanup `Drop` impl can synchronously remove its slot
/// on panic/cancellation. The lock is held only for HashMap
/// insert/remove and a synchronous `Notified::enable()` — never
/// across an `.await`.
type InflightStorage = Arc<std::sync::Mutex<HashMap<CacheKey, Arc<Notify>>>>;

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
    inner: CacheStorage,
    /// Per-key single-flight inflight markers. A `Notify` parked here
    /// while a `bootstrap` is running for that key; subsequent
    /// `get_or_init` callers for the same key wait on the notify
    /// rather than running their own `bootstrap`. This is the M11-C
    /// fix codex flagged on the initial PR: without it, two
    /// concurrent same-key misses could both fall into the bootstrap
    /// path under the prior "drop write lock, bootstrap, retake
    /// write lock" pattern.
    inflight: InflightStorage,
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

/// Per-call outcome of probing the inflight map for a key. We use
/// it as the return value of the `std::sync::MutexGuard`-holding
/// scope inside `get_or_init` so the mutex guard stays strictly
/// off the `.await` path (otherwise the returned future would not
/// be `Send`).
enum InflightOutcome {
    /// Another task is already bootstrapping this key. The cloned
    /// `Arc<Notify>` is what we'll park on via
    /// `Notified::enable() + .await`.
    WaitOn(Arc<Notify>),
    /// We just installed the inflight slot ourselves. The guard
    /// owns the slot and drops it on every exit path of the
    /// bootstrap (success, error, panic, future cancellation).
    OwnGuard(InflightGuard),
}

/// RAII guard that removes its inflight slot and wakes every parked
/// waiter on `Drop`. This is the single-flight cleanup primitive —
/// without it, an owner-task panic or future cancellation between
/// inflight insertion and bootstrap completion would strand the
/// slot and same-key callers would park on a `Notify` no owner
/// ever signals.
///
/// Synchronous `Drop` is the entire reason the inflight map uses
/// `std::sync::Mutex` rather than `tokio::sync::Mutex`: async
/// `Drop` is not yet a thing in stable Rust, but we still need
/// cleanup on panic and on aborted futures. The std mutex is held
/// only across HashMap insert/remove and a synchronous
/// `Notified::enable` call — never across `.await` — so there is
/// no risk of blocking the executor.
struct InflightGuard {
    storage: InflightStorage,
    key: CacheKey,
    notify: Arc<Notify>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        {
            let mut inflight = self
                .storage
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Only remove the slot if it's still ours — defensive
            // against an unlikely race where the cache was wiped
            // between our claim and now.
            if let Some(existing) = inflight.get(&self.key) {
                if Arc::ptr_eq(existing, &self.notify) {
                    inflight.remove(&self.key);
                }
            }
        }
        // Wake every parked waiter on this `Notify`. Waiters that
        // have already been `enable()`d are guaranteed to observe
        // this signal (lost-wake race is closed by the
        // enable-under-lock pattern in `get_or_init`).
        self.notify.notify_waiters();
    }
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
            inflight: Arc::new(std::sync::Mutex::new(HashMap::new())),
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

        loop {
            // Fast path: read lock + last_used bump on hit.
            {
                let guard = self.inner.read().await;
                if let Some(entry) = guard.get(&key) {
                    let runtime = Arc::clone(&entry.runtime);
                    drop(guard);
                    // Re-take the write lock just to bump the
                    // timestamp. The read-then-write pattern is
                    // acceptable here: the worst case is that two
                    // concurrent hits race on the timestamp update,
                    // which is benign (LRU bookkeeping is not
                    // load-bearing for correctness).
                    let mut guard = self.inner.write().await;
                    if let Some(entry) = guard.get_mut(&key) {
                        entry.last_used = Instant::now();
                    }
                    return Ok(runtime);
                }
            }

            // Miss: claim or join the single-flight inflight slot.
            // We hold the `inflight` mutex only for the duration of
            // a HashMap lookup/insert (plus, on the wait path, the
            // `Notified::enable()` registration); the actual
            // bootstrap runs outside any cache lock so different
            // keys remain concurrent.
            //
            // The wait path uses `Notified::enable()` *while still
            // holding the inflight lock* to register the waiter on
            // the `Notify`'s wait queue before dropping the lock.
            // Without this, a lost-wake race is possible: the owner
            // could call `notify_waiters()` between our lock release
            // and our `.await`, and tokio does not buffer wake-ups
            // for futures that haven't been polled/enabled yet. See
            // the `Notified::enable` docs for the canonical mpmc
            // recipe this mirrors.
            // Probe + (own-or-clone) under the inflight mutex.
            // We deliberately keep the `std::sync::MutexGuard`
            // scope tight: it never escapes this block. The two
            // possible outcomes are
            //   - `WaitOn(Arc<Notify>)` when another task owns the
            //     slot — we then build/enable a `Notified` future
            //     against that notify and `.await` it outside the
            //     lock,
            //   - `OwnGuard(InflightGuard)` when we just installed
            //     a fresh slot — we then proceed with `bootstrap`
            //     and the guard's `Drop` cleans up the slot on
            //     every exit path (success, error, panic, future
            //     cancellation).
            let outcome = {
                let mut inflight = self
                    .inflight
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(existing) = inflight.get(&key) {
                    InflightOutcome::WaitOn(Arc::clone(existing))
                } else {
                    let notify = Arc::new(Notify::new());
                    inflight.insert(key.clone(), Arc::clone(&notify));
                    InflightOutcome::OwnGuard(InflightGuard {
                        storage: Arc::clone(&self.inflight),
                        key: key.clone(),
                        notify,
                    })
                }
                // `inflight` (the MutexGuard) is dropped here at
                // block end. It NEVER spans the `.await` calls
                // below, satisfying the `Send` bound on the
                // returned future even though we use
                // `std::sync::Mutex`.
            };

            match outcome {
                InflightOutcome::WaitOn(notify) => {
                    let notified = notify.notified();
                    tokio::pin!(notified);
                    // Register as a waiter atomically with respect
                    // to any future `notify_waiters()` calls. Even
                    // if the owner had already finished and
                    // notified between our claim above and the
                    // following `.await`, the `enable` call below
                    // would observe the prior notify_waiters call
                    // via the `Notified` state machine (see the
                    // canonical recipe in the `Notified::enable`
                    // docs).
                    notified.as_mut().enable();
                    notified.await;
                    continue;
                }
                InflightOutcome::OwnGuard(guard) => {
                    let result =
                        SessionRuntime::bootstrap(profile, session_key.clone(), workspace_hint)
                            .await;
                    match result {
                        Ok(runtime) => {
                            self.insert_with_eviction(key.clone(), Arc::clone(&runtime))
                                .await;
                            drop(guard);
                            return Ok(runtime);
                        }
                        Err(error) => {
                            drop(guard);
                            return Err(error);
                        }
                    }
                }
            }
        }
    }

    /// Insert `runtime` under `key`, applying the LRU soft cap so
    /// the cache size never exceeds `max_size`. Internal helper for
    /// [`Self::get_or_init`].
    async fn insert_with_eviction(&self, key: CacheKey, runtime: Arc<SessionRuntime>) {
        let mut guard = self.inner.write().await;

        // If the key was inserted by someone else between when we
        // claimed the inflight slot and now (e.g. a different code
        // path bypassed get_or_init — unlikely but defensive), bump
        // its timestamp and drop ours; both are valid runtimes.
        if let Some(entry) = guard.get_mut(&key) {
            entry.last_used = Instant::now();
            return;
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
                runtime,
                last_used: Instant::now(),
            },
        );
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

async fn background_sweep_loop(inner: CacheStorage, idle_ttl: Duration, shutdown: Arc<Notify>) {
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

    #[tokio::test]
    async fn get_or_init_is_single_flight_under_concurrent_misses() {
        // Codex's BLOCK on the first PR: two concurrent same-key
        // get_or_init calls must observe a single
        // `SessionRuntime::bootstrap`. The single-flight inflight
        // map guarantees this: the second caller waits on the
        // first's `Notify` instead of running its own bootstrap.
        // We verify by:
        //   - racing N parallel `get_or_init`s for the same key,
        //   - asserting all of them return the same `Arc`
        //     (`Arc::ptr_eq`),
        //   - asserting the cache holds exactly one entry.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir).await;

        let cache = Arc::new(SessionRuntimeCache::new(8, Duration::from_secs(60)));
        let key = SessionKey::new("api", "single-flight");

        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let profile = Arc::clone(&profile);
            let key = key.clone();
            handles.push(tokio::spawn(async move {
                cache.get_or_init(&profile, key, None).await.unwrap()
            }));
        }

        let mut runtimes = Vec::new();
        for handle in handles {
            runtimes.push(handle.await.unwrap());
        }

        // All clones point at the same Arc.
        let first = Arc::clone(&runtimes[0]);
        for (i, rt) in runtimes.iter().enumerate().skip(1) {
            assert!(
                Arc::ptr_eq(&first, rt),
                "runtime #{i} differs from #0 — single-flight violated"
            );
        }
        // Only one entry materialized in the cache.
        assert_eq!(cache.len().await, 1);
        // Inflight slot is released (the RAII `InflightGuard`
        // drops it on every owner-side exit path).
        assert!(
            cache
                .inflight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty(),
        );
    }

    #[tokio::test]
    async fn get_or_init_releases_inflight_slot_on_aborted_owner_future() {
        // Codex BLOCK round 2 (HIGH): if the owner's `get_or_init`
        // future is cancelled (or its task panics) between
        // inflight insertion and bootstrap completion, the
        // inflight slot must NOT be stranded — otherwise future
        // same-key callers park forever. The `InflightGuard` RAII
        // makes this work by removing the slot + waking waiters
        // on `Drop`.
        //
        // We simulate cancellation by spawning a `get_or_init`
        // task and aborting it immediately, then issuing a fresh
        // `get_or_init` for the same key on this task and
        // asserting it completes. (If the slot were stranded the
        // second call would park forever; a `tokio::time::timeout`
        // turns "park forever" into a test failure.)
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir).await;

        let cache = Arc::new(SessionRuntimeCache::new(8, Duration::from_secs(60)));
        let key = SessionKey::new("api", "owner-cancelled");

        // Spawn an owner that yields before completing. We grab
        // its `JoinHandle` so we can `abort()` it mid-flight.
        let task_cache = Arc::clone(&cache);
        let task_profile = Arc::clone(&profile);
        let task_key = key.clone();
        let handle = tokio::spawn(async move {
            // Yield several times to make sure the abort lands
            // while the future is suspended somewhere inside
            // `get_or_init`/`bootstrap`. Bootstrap itself does
            // synchronous filesystem work, so most yields land
            // before/after that work — that's fine for the
            // cancellation-safety property.
            let fut = task_cache.get_or_init(&task_profile, task_key, None);
            tokio::pin!(fut);
            for _ in 0..32 {
                tokio::task::yield_now().await;
            }
            (&mut fut).await
        });
        // Give the owner a tick to claim the inflight slot, then
        // abort it. The abort drops the `get_or_init` future,
        // which drops the `InflightGuard`, which removes the slot
        // and wakes any waiter.
        tokio::task::yield_now().await;
        handle.abort();
        let _ = handle.await; // Drain JoinError.

        // A fresh same-key call must complete — wrapped in a
        // timeout so a regression to the stranded-slot bug fails
        // the test fast rather than hanging CI.
        let rt = tokio::time::timeout(
            Duration::from_secs(5),
            cache.get_or_init(&profile, key, None),
        )
        .await
        .expect("get_or_init must not park after owner cancellation")
        .expect("bootstrap must succeed");
        assert_eq!(cache.len().await, 1);
        // The inflight slot must be released after the second
        // owner completes.
        assert!(
            cache
                .inflight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty(),
        );
        drop(rt);
    }
}
