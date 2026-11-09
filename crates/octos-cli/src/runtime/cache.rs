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
use octos_agent::{EffectivePermissions, SandboxConfig};
use octos_core::SessionKey;
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinHandle;

use super::{ProfileRuntime, SessionRuntime};

/// How often the background sweep task scans for idle entries.
///
/// 60 s strikes the balance between "leaks one minute of capacity
/// after the last hit" and "wakes the executor more often than the
/// hit rate justifies". Tests may override the cache TTL but the
/// sweep cadence is fixed.
const BACKGROUND_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Default total heuristic weight budget for cached session runtimes (256 MiB).
///
/// This is an eviction accounting unit, NOT a promise about process RSS.
pub const DEFAULT_BYTE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// Fixed heuristic weight for the non-transcript parts of one SessionRuntime.
/// Agent/tool/sandbox structures vary by profile and allocator; 4 MiB is a
/// deliberately coarse admission weight, not a measured resident-size claim.
const SESSION_RUNTIME_BASE_BYTES: u64 = 4 * 1024 * 1024;

/// Cache key shared by every storage map in this module. Triples the
/// profile id (from [`ProfileRuntime::profile_id`]), the session key,
/// and the resolved **sessions root** so a profile reload only
/// invalidates entries belonging to that profile.
///
/// The `PathBuf` third element is the on-disk store root a runtime is
/// bootstrapped against ([`resolve_sessions_root_from_hint`]). It is
/// **constant** (`profile.data_dir`) while `appui.sessions_in_cwd` is off, so
/// keying by it is byte-identical to the historical `(profile, session_key)`
/// pair in that mode. With the flag on it makes the cache identity distinguish
/// the SAME `SessionKey` opened against DIFFERENT project cwds: same key + same
/// cwd stays single-flight (one runtime), but same key + different cwd
/// resolves to SEPARATE runtimes/roots — closing the corruption where a
/// re-open in project B was handed project A's cached runtime (and then ran
/// tools / wrote history under A's root).
type CacheKey = (String, SessionKey, PathBuf);

/// Storage shape for the main cache. Entries and their total heuristic weight
/// live under ONE lock so eviction/invalidation cannot drift the accounting.
type CacheStorage = Arc<tokio::sync::RwLock<CacheState>>;

/// Storage shape for the per-key single-flight inflight map.
/// Uses [`std::sync::Mutex`] (not [`tokio::sync::Mutex`]) so the
/// owner-side cleanup `Drop` impl can synchronously remove its slot
/// on panic/cancellation. The lock is held only for HashMap
/// insert/remove — never across an `.await`.
///
/// The inflight value is a [`tokio::sync::Semaphore`] starting with
/// zero permits. The owner calls `close()` on completion (success,
/// error, panic, future cancellation), at which point every parked
/// waiter's `acquire().await` returns `Err(AcquireError)` and a
/// freshly-issued `acquire().await` on the already-closed
/// semaphore returns `Err` on first poll. The "closed" state is
/// sticky, so there is no lost-wake race regardless of whether the
/// waiter registers before or after the close call.
type InflightStorage = Arc<std::sync::Mutex<HashMap<CacheKey, Arc<Semaphore>>>>;

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
/// - **`byte_budget`** — a total heuristic weight budget. Insertion and cache
///   hits evict LRU entries until the weight fits, except that the newest/last
///   entry is always retained even when its own weight exceeds the budget.
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
    /// Per-key single-flight inflight slots. A `Semaphore` parked
    /// here while a `bootstrap` is running for that key; subsequent
    /// `get_or_init` callers for the same key `acquire().await`
    /// against it rather than running their own `bootstrap`. The
    /// owner's `InflightGuard::drop` closes the semaphore (waking
    /// every parked waiter with `Err(AcquireError)`) on every exit
    /// path, including panic and future cancellation — see the
    /// [`InflightStorage`] type alias for the lost-wake-race
    /// reasoning behind picking `Semaphore` over `Notify`.
    inflight: InflightStorage,
    /// Per-profile invalidation generation. Bumped by
    /// [`Self::invalidate_profile`]; a bootstrap that STARTED before the bump
    /// (holding the old `ProfileRuntime`) must not insert its stale runtime
    /// after the sweep — it serves its own caller uncached instead.
    generations: Arc<std::sync::Mutex<HashMap<String, u64>>>,
    /// Per-SESSION invalidation generation. Bumped by
    /// [`Self::invalidate_session`] on a permission change; a bootstrap that
    /// STARTED before the bump (holding the pre-change permissions) must not
    /// insert its now-stale runtime after the sweep. Session-scoped rather
    /// than profile-scoped because a permission change targets one session
    /// but its runtime may be cached under any of several profile ids.
    session_generations: Arc<std::sync::Mutex<HashMap<SessionKey, u64>>>,
    max_size: usize,
    idle_ttl: Duration,
    byte_budget: u64,
    /// Process-global `appui.sessions_in_cwd` flag, threaded verbatim into
    /// every [`SessionRuntime::bootstrap_with_permissions_and_sandbox`] this
    /// cache performs. Held on the cache (constructed once per `octos serve`)
    /// so the flag has a single source of truth and `get_or_init*` needs no
    /// per-call parameter. When set, a cwd-hinted session's transcript store
    /// relocates to `<cwd>/.octos`; no-hint sessions are unaffected. Default
    /// `false` (legacy per-profile storage).
    sessions_in_cwd: bool,
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
    /// Coarse cache-admission weight: runtime base + current cached transcript.
    /// Not an RSS measurement.
    estimated_bytes: u64,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    estimated_bytes: u64,
}

impl CacheState {
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn get(&self, key: &CacheKey) -> Option<&CacheEntry> {
        self.entries.get(key)
    }

    fn get_mut(&mut self, key: &CacheKey) -> Option<&mut CacheEntry> {
        self.entries.get_mut(key)
    }

    fn insert(&mut self, key: CacheKey, entry: CacheEntry) -> Option<CacheEntry> {
        let weight = entry.estimated_bytes;
        let previous = self.entries.insert(key, entry);
        self.estimated_bytes = self.estimated_bytes.saturating_add(weight);
        if let Some(previous) = &previous {
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_sub(previous.estimated_bytes);
        }
        previous
    }

    fn remove(&mut self, key: &CacheKey) -> Option<CacheEntry> {
        let removed = self.entries.remove(key);
        if let Some(removed) = &removed {
            self.estimated_bytes = self.estimated_bytes.saturating_sub(removed.estimated_bytes);
        }
        removed
    }

    fn retain(&mut self, mut keep: impl FnMut(&CacheKey, &CacheEntry) -> bool) {
        let mut removed_weight = 0u64;
        self.entries.retain(|key, entry| {
            if keep(key, entry) {
                true
            } else {
                removed_weight = removed_weight.saturating_add(entry.estimated_bytes);
                false
            }
        });
        self.estimated_bytes = self.estimated_bytes.saturating_sub(removed_weight);
    }

    fn update_weight(&mut self, key: &CacheKey, estimated_bytes: u64) {
        if let Some(entry) = self.entries.get_mut(key) {
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_sub(entry.estimated_bytes)
                .saturating_add(estimated_bytes);
            entry.estimated_bytes = estimated_bytes;
        }
    }

    fn evict_lru_until_within_budget(&mut self, byte_budget: u64) {
        while self.entries.len() > 1 && self.estimated_bytes > byte_budget {
            let Some(lru_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.remove(&lru_key);
        }
    }

    fn iter(&self) -> std::collections::hash_map::Iter<'_, CacheKey, CacheEntry> {
        self.entries.iter()
    }

    fn iter_mut(&mut self) -> std::collections::hash_map::IterMut<'_, CacheKey, CacheEntry> {
        self.entries.iter_mut()
    }

    fn keys(&self) -> std::collections::hash_map::Keys<'_, CacheKey, CacheEntry> {
        self.entries.keys()
    }

    fn values(&self) -> std::collections::hash_map::Values<'_, CacheKey, CacheEntry> {
        self.entries.values()
    }
}

/// Per-call outcome of probing the inflight map for a key. We use
/// it as the return value of the `std::sync::MutexGuard`-holding
/// scope inside `get_or_init` so the mutex guard stays strictly
/// off the `.await` path (otherwise the returned future would not
/// be `Send`).
enum InflightOutcome {
    /// Another task is already bootstrapping this key. We cloned
    /// the slot's [`Semaphore`] under the lock; when the owner
    /// finishes (or its guard drops), it calls `close()` on the
    /// semaphore. Our `acquire().await` then returns `Err`
    /// regardless of whether registration happens before or after
    /// the close call — see the `Semaphore::close` docs for the
    /// sticky-closed semantics this relies on.
    WaitOn(Arc<Semaphore>),
    /// We just installed the inflight slot ourselves. The guard
    /// owns the slot and drops it on every exit path of the
    /// bootstrap (success, error, panic, future cancellation).
    OwnGuard(InflightGuard),
}

/// RAII guard that removes its inflight slot and closes the slot's
/// semaphore (waking every parked waiter) on `Drop`. This is the
/// single-flight cleanup primitive — without it, an owner-task
/// panic or future cancellation between inflight insertion and
/// bootstrap completion would strand the slot and same-key callers
/// would park on a semaphore no owner ever closes.
///
/// Synchronous `Drop` is the entire reason the inflight map uses
/// `std::sync::Mutex` rather than `tokio::sync::Mutex`: async
/// `Drop` is not yet a thing in stable Rust, but we still need
/// cleanup on panic and on aborted futures. The std mutex is held
/// only across HashMap insert/remove — never across `.await` — so
/// there is no risk of blocking the executor.
struct InflightGuard {
    storage: InflightStorage,
    key: CacheKey,
    semaphore: Arc<Semaphore>,
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
                if Arc::ptr_eq(existing, &self.semaphore) {
                    inflight.remove(&self.key);
                }
            }
        }
        // Close the semaphore. Every parked `acquire` future
        // (and every future `acquire` call on this Arc) returns
        // `Err(AcquireError)` from this point on. The closed
        // state is sticky, so there is no lost-wake race even if
        // a waiter registered after we returned from this Drop.
        self.semaphore.close();
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
        let inner = Arc::new(tokio::sync::RwLock::new(CacheState::default()));
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
            generations: Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_generations: Arc::new(std::sync::Mutex::new(HashMap::new())),
            max_size,
            idle_ttl,
            byte_budget: DEFAULT_BYTE_BUDGET_BYTES,
            // Default OFF: legacy per-profile storage. `octos serve` opts in
            // via `with_sessions_in_cwd(config.appui.sessions_in_cwd)`.
            sessions_in_cwd: false,
            shutdown,
            sweep_task: std::sync::Mutex::new(sweep_task),
        }
    }

    /// Enable per-project (`appui.sessions_in_cwd`) session storage for every
    /// runtime this cache bootstraps. Wired by `octos serve` from
    /// `config.appui.sessions_in_cwd`; off by default so all existing call
    /// sites (and tests) keep legacy per-profile storage.
    pub fn with_sessions_in_cwd(mut self, sessions_in_cwd: bool) -> Self {
        self.sessions_in_cwd = sessions_in_cwd;
        self
    }

    /// Override the total heuristic weight budget. A zero budget is valid for
    /// tests and constrained hosts: the cache still retains its newest entry.
    pub fn with_byte_budget_bytes(mut self, byte_budget: u64) -> Self {
        self.byte_budget = byte_budget;
        self
    }

    /// Configured total heuristic weight budget (not an RSS limit).
    pub fn byte_budget(&self) -> u64 {
        self.byte_budget
    }

    /// Whether this cache relocates cwd-hinted sessions to `<cwd>/.octos`.
    /// Read by the `session/list` handler to decide whether an incoming
    /// `cwd` param scopes the listing (single source of truth with the
    /// bootstrap path).
    pub fn sessions_in_cwd(&self) -> bool {
        self.sessions_in_cwd
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
        // Fixed workspace-write default (no mutable-store resolution to race
        // against): sampling the epoch here is correct.
        let permissions_epoch = self.session_generation(&session_key);
        self.get_or_init_with_permissions(
            profile,
            session_key,
            workspace_hint,
            EffectivePermissions::workspace_write(),
            permissions_epoch,
        )
        .await
    }

    /// Look up or build a session runtime using explicit effective
    /// permissions. Callers should invalidate a cached session before changing
    /// permission policy; cache hits intentionally keep the first bootstrapped
    /// runtime for a session key.
    pub async fn get_or_init_with_permissions(
        &self,
        profile: &Arc<ProfileRuntime>,
        session_key: SessionKey,
        workspace_hint: Option<PathBuf>,
        permissions: EffectivePermissions,
        // Epoch captured by the caller BEFORE it resolved `permissions` — see
        // `get_or_init_with_permissions_and_sandbox`. REQUIRED (not
        // self-sampled) so every permission-bearing call site is forced to
        // pair its snapshot with the pre-resolution epoch; a self-sample here
        // could pair a post-downgrade epoch with pre-downgrade permissions on
        // a preempted multi-threaded caller (codex P1 round 4 on #1639).
        permissions_epoch: u64,
    ) -> Result<Arc<SessionRuntime>> {
        self.get_or_init_with_permissions_and_sandbox(
            profile,
            session_key,
            workspace_hint,
            permissions,
            None,
            permissions_epoch,
        )
        .await
    }

    /// Look up or build a session runtime using explicit effective
    /// permissions plus an optional, already-validated sandbox override.
    /// Cache hits intentionally keep the first bootstrapped runtime for a
    /// session key, including its sandbox policy.
    pub async fn get_or_init_with_permissions_and_sandbox(
        &self,
        profile: &Arc<ProfileRuntime>,
        session_key: SessionKey,
        workspace_hint: Option<PathBuf>,
        permissions: EffectivePermissions,
        sandbox_override: Option<SandboxConfig>,
        // Session-generation epoch captured by the CALLER at (or before) the
        // moment it resolved `permissions` — NOT re-sampled here. A bootstrap
        // that waits on an in-flight slot and then claims its own must reject
        // its insert if an `invalidate_session` bumped the generation any time
        // after the permissions were resolved, or it would re-cache the
        // pre-change (possibly dangerous) policy after a downgrade (codex P1
        // round 3 on #1639).
        permissions_epoch: u64,
    ) -> Result<Arc<SessionRuntime>> {
        // Resolve the sessions root the same way bootstrap will, so the cache
        // identity separates the same key opened against different project
        // cwds (flag on) while staying constant — `profile.data_dir` — for
        // every session when the flag is off (byte-identical legacy keying).
        let key_root = super::session::resolve_sessions_root_from_hint(
            profile,
            workspace_hint.as_deref(),
            self.sessions_in_cwd,
        );
        let key = (profile.profile_id.clone(), session_key.clone(), key_root);

        loop {
            // Fast path: a matching profile allocation may reuse the cached
            // session. The profile id alone is insufficient because runtime
            // replacement intentionally keeps that stable.
            let cache_hit = {
                let mut guard = self.inner.write().await;
                // A replacement ProfileRuntime may also move the profile's
                // data directory, which changes the sessions-root component
                // of every cache key. Remove entries owned by the previous
                // allocation across all roots before probing the new key.
                guard.retain(|(profile_id, _, _), entry| {
                    profile_id != &profile.profile_id
                        || Arc::ptr_eq(&entry.runtime.profile, profile)
                });
                let mut hit = None;
                if let Some(entry) = guard.get_mut(&key) {
                    if Arc::ptr_eq(&entry.runtime.profile, profile) {
                        // UPCR-2026-035: a runtime cached before its session
                        // was bound (or rebound) to an app is stale — it
                        // carries the profile's memory, its workspace and its
                        // permissions. Drop it and rebuild.
                        if entry.runtime.app_binding_is_current() {
                            entry.last_used = Instant::now();
                            hit = Some(Arc::clone(&entry.runtime));
                        } else {
                            guard.remove(&key);
                        }
                    }
                }
                hit
            };
            if let Some(runtime) = cache_hit {
                self.refresh_entry_weight(&key, &runtime).await;
                return Ok(runtime);
            }

            // Miss: claim or join the single-flight inflight slot.
            // We hold the `inflight` std mutex only for the
            // duration of a HashMap lookup/insert; the actual
            // bootstrap (and `acquire().await` on the wait path)
            // runs outside the lock so different keys remain
            // concurrent.
            //
            // Probe + (own-or-clone) under the inflight mutex. The
            // `std::sync::MutexGuard` scope is tight: it never
            // escapes this block, so it never spans an `.await`
            // and the returned future remains `Send`. The two
            // possible outcomes are
            //   - `WaitOn(Arc<Semaphore>)` when another task owns
            //     the slot — we await `acquire()`, which returns
            //     `Err(AcquireError)` once the owner closes the
            //     semaphore. Tokio's `Semaphore::close` semantics
            //     are sticky, so a `close()` that happened before
            //     our registration still fails the next poll —
            //     no lost-wake race.
            //   - `OwnGuard(InflightGuard)` when we just installed
            //     a fresh slot — we then proceed with `bootstrap`
            //     and the guard's `Drop` cleans up the slot
            //     (closes the semaphore + removes the map entry)
            //     on every exit path: success, error, panic, or
            //     future cancellation.
            let outcome = {
                let mut inflight = self
                    .inflight
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(existing) = inflight.get(&key) {
                    InflightOutcome::WaitOn(Arc::clone(existing))
                } else {
                    let semaphore = Arc::new(Semaphore::new(0));
                    inflight.insert(key.clone(), Arc::clone(&semaphore));
                    InflightOutcome::OwnGuard(InflightGuard {
                        storage: Arc::clone(&self.inflight),
                        key: key.clone(),
                        semaphore,
                    })
                }
                // `inflight` (the MutexGuard) is dropped here at
                // block end. It NEVER spans the `.await` calls
                // below, satisfying the `Send` bound on the
                // returned future even though we use
                // `std::sync::Mutex`.
            };

            match outcome {
                InflightOutcome::WaitOn(semaphore) => {
                    // We expect `acquire()` to return `Err` once
                    // the owner closes the semaphore (on success,
                    // error, panic, or future cancellation). The
                    // error arm is the only arm we care about —
                    // either way we loop back to the fast-path
                    // read and pick up the inserted runtime (if
                    // bootstrap succeeded) or re-attempt the
                    // bootstrap ourselves (if it failed).
                    let _ = semaphore.acquire().await;
                    continue;
                }
                InflightOutcome::OwnGuard(guard) => {
                    let generation_at_start = self.profile_generation(&key.0);
                    // Caller-supplied epoch (tied to the permission snapshot),
                    // not a post-wait re-sample — see the param doc.
                    let session_generation_at_start = permissions_epoch;
                    let result = SessionRuntime::bootstrap_with_permissions_and_sandbox(
                        profile,
                        session_key.clone(),
                        workspace_hint,
                        permissions,
                        sandbox_override,
                        // Process-global flag (default false). A cwd-hinted
                        // session relocates its store to `<cwd>/.octos` only
                        // when this is on; no-hint sessions ignore it.
                        self.sessions_in_cwd,
                    )
                    .await;
                    match result {
                        Ok(runtime) => {
                            // `insert_with_eviction` returns the
                            // canonical `Arc<SessionRuntime>` for
                            // this key — either the one we just
                            // built, or the one another task
                            // already inserted (e.g. a prior
                            // single-flight era completed and
                            // dropped its slot in the window
                            // between our fast-path miss and our
                            // own slot claim). Returning the
                            // canonical Arc is what makes
                            // single-flight a true "one cached
                            // runtime per key" invariant rather
                            // than a "one bootstrap per inflight
                            // era" invariant.
                            let canonical = self
                                .insert_with_eviction(
                                    key.clone(),
                                    runtime,
                                    generation_at_start,
                                    session_generation_at_start,
                                )
                                .await;
                            drop(guard);
                            return Ok(canonical);
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

    /// Refresh a hit's transcript weight without holding the cache lock while
    /// waiting for the SessionManager lock (delete takes those in the opposite
    /// order). If invalidation/replacement wins meanwhile, there is nothing to
    /// update and the already-returned Arc remains valid outside the cache.
    async fn refresh_entry_weight(&self, key: &CacheKey, runtime: &Arc<SessionRuntime>) {
        let transcript_bytes = {
            let sessions = runtime.sessions.lock().await;
            sessions.estimated_cached_transcript_bytes(&runtime.session_key)
        };
        let estimated_bytes = SESSION_RUNTIME_BASE_BYTES.saturating_add(transcript_bytes);
        let mut guard = self.inner.write().await;
        if guard
            .get(key)
            .is_some_and(|entry| Arc::ptr_eq(&entry.runtime, runtime))
        {
            guard.update_weight(key, estimated_bytes);
            guard.evict_lru_until_within_budget(self.byte_budget);
        }
    }

    /// Insert `runtime` under `key`, applying the LRU soft cap so
    /// the cache size never exceeds `max_size`. Returns the
    /// canonical `Arc<SessionRuntime>` for the key — either the
    /// `runtime` we just inserted, or an existing entry's runtime
    /// if the cache already had one under this key.
    ///
    /// The "canonical Arc" return value is load-bearing for the
    /// single-flight invariant: if a prior single-flight era for
    /// the same key completed and dropped its slot between the
    /// current caller's fast-path miss and its own slot claim,
    /// the current caller would otherwise return its own freshly
    /// bootstrapped runtime to its caller, leaving two distinct
    /// `Arc<SessionRuntime>`s for the same `(profile_id,
    /// session_key)` pair in flight. Returning the cached entry's
    /// runtime collapses both back onto the canonical Arc.
    async fn insert_with_eviction(
        &self,
        key: CacheKey,
        runtime: Arc<SessionRuntime>,
        generation_at_start: u64,
        session_generation_at_start: u64,
    ) -> Arc<SessionRuntime> {
        let mut guard = self.inner.write().await;
        // Re-check BOTH generations UNDER the map lock: each sweep
        // (`invalidate_profile` / `invalidate_session`) serializes on this
        // same lock and its bump happens-before its sweep, so either we
        // observe the bump here (and skip caching the stale runtime) or our
        // insert lands first and the sweep removes it — a stale runtime can
        // never survive a profile switch OR a permission change.
        if self.profile_generation(&key.0) != generation_at_start
            || self.session_generation(&key.1) != session_generation_at_start
        {
            return runtime;
        }

        // The profile allocation may have changed while this key was being
        // bootstrapped. Its sessions root is part of the key, so evict stale
        // allocations across every root rather than only the exact key below.
        guard.retain(|(profile_id, _, _), entry| {
            profile_id != &runtime.profile.profile_id
                || Arc::ptr_eq(&entry.runtime.profile, &runtime.profile)
        });

        // If a runtime is already present (e.g. another task
        // bootstrapped in a prior single-flight era and inserted
        // before our claim), bump its timestamp and return its
        // `Arc` — the caller will hand THAT Arc back, not the
        // redundant one we built.
        let existing = guard.get_mut(&key).and_then(|entry| {
            if Arc::ptr_eq(&entry.runtime.profile, &runtime.profile) {
                entry.last_used = Instant::now();
                Some(Arc::clone(&entry.runtime))
            } else {
                None
            }
        });
        if let Some(canonical) = existing {
            drop(guard);
            self.refresh_entry_weight(&key, &canonical).await;
            return canonical;
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

        let canonical = Arc::clone(&runtime);
        guard.insert(
            key,
            CacheEntry {
                runtime,
                last_used: Instant::now(),
                estimated_bytes: SESSION_RUNTIME_BASE_BYTES,
            },
        );
        guard.evict_lru_until_within_budget(self.byte_budget);
        canonical
    }

    /// Drop every cached entry for `(profile_id, session_key)` regardless of
    /// the resolved sessions root. Used by M11-D's
    /// `/api/sessions/:id/delete` handler and by the config watcher when a
    /// profile reload invalidates every cached session for the profile.
    ///
    /// Root-agnostic on purpose: a session key can now be cached under more
    /// than one root (the same key opened against two project cwds with
    /// `appui.sessions_in_cwd` on), and an invalidation of that logical
    /// session must clear all of them.
    ///
    /// Idempotent: removing an absent key is a no-op.
    pub async fn invalidate(&self, profile_id: &str, session_key: &SessionKey) {
        let mut guard = self.inner.write().await;
        guard.retain(|(key_profile, key_session, _), _| {
            key_profile != profile_id || key_session != session_key
        });
    }

    /// Drop every cached session runtime belonging to `profile_id`. Used when
    /// a profile's LLM selection changes at runtime (`profile/llm/select` /
    /// upsert): cached `SessionRuntime`s embed the OLD provider chain, so the
    /// next turn must re-materialize against the rebuilt `ProfileRuntime`.
    pub async fn invalidate_profile(&self, profile_id: &str) {
        // Bump the generation FIRST: any in-flight bootstrap that started
        // against the old ProfileRuntime observes the change at insert time
        // and skips caching its stale runtime.
        {
            let mut generations = self
                .generations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *generations.entry(profile_id.to_owned()).or_insert(0) += 1;
        }
        let mut guard = self.inner.write().await;
        guard.retain(|(key_profile, _, _), _| key_profile != profile_id);
    }

    fn profile_generation(&self, profile_id: &str) -> u64 {
        self.generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(profile_id)
            .copied()
            .unwrap_or(0)
    }

    /// Drop every cached runtime for `session_key` REGARDLESS of profile, and
    /// bump its session generation so an in-flight bootstrap (started before
    /// this call, holding pre-change permissions) skips caching at insert.
    /// Used on a permission change (`permission/profile/set`): the runtime
    /// embeds the old sandbox/network policy, and the connection may not know
    /// which profile id the session was opened under (codex P1 ×2 on #1639).
    pub async fn invalidate_session(&self, session_key: &SessionKey) {
        {
            let mut generations = self
                .session_generations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *generations.entry(session_key.clone()).or_insert(0) += 1;
        }
        let mut guard = self.inner.write().await;
        guard.retain(|(_, key_session, _), _| key_session != session_key);
    }

    /// Drop every cached runtime whose session has `topic`, under any base
    /// key and profile, bumping each one's session generation. Used when a
    /// topic becomes (or changes) an app binding (UPCR-2026-034/035): a
    /// runtime built before the binding carries the profile's memory,
    /// workspace and permissions.
    pub async fn invalidate_sessions_with_topic(&self, topic: &str) {
        let mut guard = self.inner.write().await;
        let stale: Vec<SessionKey> = guard
            .keys()
            .filter(|(_, key, _)| key.topic() == Some(topic))
            .map(|(_, key, _)| key.clone())
            .collect();
        {
            let mut generations = self
                .session_generations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for key in &stale {
                *generations.entry(key.clone()).or_insert(0) += 1;
            }
        }
        guard.retain(|(_, key, _), _| key.topic() != Some(topic));
    }

    pub(crate) fn session_generation(&self, session_key: &SessionKey) -> u64 {
        self.session_generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_key)
            .copied()
            .unwrap_or(0)
    }

    /// Mark every cached runtime for `session` as used right now, so the
    /// idle sweep does not evict a Session a client is holding open.
    ///
    /// Rebuilding an evicted runtime is correct but slow — a long session
    /// costs seconds to reload from disk, which a UI client pays on its next
    /// `session/open` (and can hit its request timeout). A connection with
    /// the Session open calls this periodically, well inside the idle TTL.
    /// Returns how many cached entries were refreshed (0 when the Session is
    /// not cached, which needs no keep-alive).
    pub async fn keep_session_alive(&self, session: &SessionKey) -> usize {
        let mut guard = self.inner.write().await;
        let mut refreshed = 0usize;
        for (_, entry) in guard.iter_mut().filter(|((_, key, _), _)| key == session) {
            entry.last_used = Instant::now();
            refreshed += 1;
        }
        refreshed
    }

    /// Release the `client_commands` declarations connection `owner` made on
    /// any cached session, when that connection closes.
    pub async fn release_client_commands(&self, owner: u64) {
        let guard = self.inner.read().await;
        for entry in guard.values() {
            entry.runtime.release_client_commands(owner);
        }
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
        drop(guard);

        // A generation is needed only while an older bootstrap can still
        // finish and attempt insertion. Hold the inflight map while pruning so
        // no new bootstrap can slip into the snapshot; active bootstraps keep
        // their session generation and the stale-insert guard remains intact.
        let inflight = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
        let mut generations = self
            .session_generations
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        generations.retain(|session, _| {
            inflight
                .keys()
                .any(|(_, inflight_session, _)| inflight_session == session)
        });
    }

    /// Current total heuristic weight. This is deliberately NOT a precise
    /// heap/RSS measurement; it is the cache's eviction accounting unit.
    pub async fn estimated_bytes(&self) -> u64 {
        self.inner.read().await.estimated_bytes
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
