//! Module 3 — [`FleetWorkerPool`]: the bounded launcher.
//!
//! `dispatch` is *preflight → launch → permit-bounded background run*:
//!
//! 1. **Preflight (before any durable state):** read the task's brief from the
//!    fleet view and create its working dir. Doing this BEFORE `launch_child`
//!    means a failure leaves NO durable attempt — a bad `workspace_root` or a
//!    missing task can never wedge a child in `Launching` (P1-4a).
//! 2. **Launch:** CAS a `Ready` child to launched (a typed [`LaunchOutcome`],
//!    never an error).
//! 3. On `Launched`, arm a **drop-guard** over the `[launch → complete]` region
//!    — SYNCHRONOUSLY, with no `.await` between `launch_child` and arming it,
//!    then MOVE it into the spawned future (P1-4a) so even a future dropped
//!    before its first poll (`dispatch().await` then `handle.abort()` on a
//!    current-thread runtime) still fires the guard's `Terminated` cleanup and
//!    can't leave the child `Launching`. The spawned run then acquires the
//!    PER-FLEET permit BEFORE the global permit (P2-1: a task blocked on its
//!    fleet's bound must not hold a global slot and head-of-line starve other
//!    fleets) and runs the attempt holding both; it disarms the guard only when
//!    the attempt is settled-or-not-ours, keeping it armed on a `RecordError`
//!    (P1-4b).
//!
//! Concurrency is clamped to ≥1 at construction (P2-1: a zero permit would
//! launch-then-wait-forever). Production drops the returned [`JoinHandle`]
//! (launch-and-return); tests await it.
//!
//! **Shutdown residual (P1-4c):** the guard's cleanup is spawned only if a
//! Tokio runtime is in scope ([`tokio::runtime::Handle::try_current`], so a
//! sync drop never panics); during runtime/process shutdown it may not run, and
//! recovery then falls to BOOT reconciliation of the stale lease (the owning
//! daemon's contract, a later PR).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;

use octos_core::safe_filename;
use octos_fleet::{AcceptanceVerdict, ChildResultSnapshot, Fleet, FleetKernelStore, LaunchOutcome};

use crate::{AgentFactory, AttemptOutcome, run_attempt};

/// Static configuration for a [`FleetWorkerPool`].
///
/// The agent-loop knobs (`max_iterations` / `max_tokens`) live on the
/// [`AgentFactory`], the single source of truth for how an attempt's agent
/// is built — they are deliberately NOT duplicated here.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Max attempts running across ALL fleets at once.
    pub global_concurrency: usize,
    /// Max attempts running per single fleet at once.
    pub per_fleet_concurrency: usize,
    /// Hard wall-clock deadline for one attempt's agent run.
    pub deadline: Duration,
    /// This daemon boot's lease owner epoch (fences stale completions).
    pub owner_epoch: u64,
    /// Lease TTL stamped at launch (recovery reclaims an expired lease).
    pub lease_ttl_ms: u64,
    /// Tokens reserved on the fleet budget at launch (soft admission).
    pub projected_tokens: u64,
    /// Base directory under which each attempt gets its own working dir
    /// (`<workspace_root>/<fleet>/<task>`), used as the tool cwd + the
    /// acceptance-gate root.
    pub workspace_root: PathBuf,
}

/// The result of a [`FleetWorkerPool::dispatch`]: the typed launch decision
/// plus, on `Launched`, the background run's [`JoinHandle`]. A `Rejected*`
/// launch carries `handle: None` (no work was spawned).
#[derive(Debug)]
pub struct Dispatched {
    pub launch: LaunchOutcome,
    pub handle: Option<JoinHandle<AttemptOutcome>>,
}

/// A bounded pool of closed task-workers over one [`FleetKernelStore`].
pub struct FleetWorkerPool {
    store: Arc<FleetKernelStore>,
    factory: Arc<AgentFactory>,
    cfg: PoolConfig,
    global_sem: Arc<Semaphore>,
    /// Per-fleet permit semaphores, shared into each spawned run so the
    /// (awaiting) lookup happens INSIDE the guarded future, not before it
    /// (P1-4a). `Arc` so the spawned `'static` future can hold it.
    per_fleet: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl FleetWorkerPool {
    /// Build a pool. `clock` supplies wall-clock milliseconds for launch and
    /// completion settlement.
    pub fn new(
        store: Arc<FleetKernelStore>,
        factory: Arc<AgentFactory>,
        cfg: PoolConfig,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        // P2-1: a zero permit would launch the child then wait forever on a
        // permit that never frees. Clamp to ≥1 so the pool always makes
        // progress; a misconfigured `0` self-heals to serial execution.
        if cfg.global_concurrency == 0 || cfg.per_fleet_concurrency == 0 {
            tracing::warn!(
                global = cfg.global_concurrency,
                per_fleet = cfg.per_fleet_concurrency,
                "fleet worker pool: zero concurrency clamped to 1 (would otherwise hang)",
            );
        }
        let global_sem = Arc::new(Semaphore::new(cfg.global_concurrency.max(1)));
        Self {
            store,
            factory,
            cfg,
            global_sem,
            per_fleet: Arc::new(Mutex::new(HashMap::new())),
            clock,
        }
    }

    /// Get-or-create a fleet's permit semaphore in `map` (clamped ≥1, P2-1).
    /// A free fn over the shared `map` + concurrency so the spawned run can do
    /// the lookup INSIDE its guarded future (P1-4a) without borrowing `self`.
    async fn per_fleet_sem(
        map: &Mutex<HashMap<String, Arc<Semaphore>>>,
        fleet_id: &str,
        per_fleet_concurrency: usize,
    ) -> Arc<Semaphore> {
        map.lock()
            .await
            .entry(fleet_id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(per_fleet_concurrency.max(1))))
            .clone()
    }

    /// Launch a ready task and, on success, spawn its permit-bounded run.
    ///
    /// Returns immediately with the typed [`LaunchOutcome`]; on `Launched`
    /// the [`Dispatched::handle`] resolves to the attempt's [`AttemptOutcome`]
    /// once the background run finishes.
    ///
    /// Fallible PREFLIGHT (reading the task view + creating the working dir)
    /// runs BEFORE `launch_child`, so a failure returns an `Err` with NO
    /// durable attempt — the child stays `Ready` and is never wedged in
    /// `Launching` (P1-4a).
    pub async fn dispatch(&self, fleet_id: &str, task_id: &str) -> Result<Dispatched> {
        // ---- PREFLIGHT (before any durable state) ----
        let view = Fleet::bind(self.store.clone(), fleet_id.to_string())
            .view()
            .await?;
        let Some(task_view) = view.tasks.into_iter().find(|t| t.task_id == task_id) else {
            return Err(eyre!(
                "task {task_id} is absent from fleet {fleet_id}'s plan"
            ));
        };
        let working_dir = self
            .cfg
            .workspace_root
            .join(safe_filename(fleet_id))
            .join(safe_filename(task_id));
        tokio::fs::create_dir_all(&working_dir).await.map_err(|e| {
            eyre!(
                "preflight: create working dir {} failed: {e}",
                working_dir.display()
            )
        })?;

        // ---- LAUNCH (durable state) ----
        let now = (self.clock)();
        let launch = self
            .store
            .launch_child(
                fleet_id,
                task_id,
                self.cfg.projected_tokens,
                now,
                self.cfg.owner_epoch,
                self.cfg.lease_ttl_ms,
            )
            .await?;

        let LaunchOutcome::Launched { attempt_id } = &launch else {
            // RejectedNotReady / RejectedDoubleLaunch / RejectedBudgetExceeded:
            // no work is spawned.
            return Ok(Dispatched {
                launch,
                handle: None,
            });
        };
        let attempt_id = attempt_id.clone();

        let store = self.store.clone();
        let factory = self.factory.clone();
        let clock = self.clock.clone();
        let global_sem = self.global_sem.clone();
        let per_fleet = self.per_fleet.clone();
        let per_fleet_concurrency = self.cfg.per_fleet_concurrency;
        let deadline = self.cfg.deadline;
        let owner_epoch = self.cfg.owner_epoch;
        let fleet_id = fleet_id.to_string();
        let task_id = task_id.to_string();

        // P1-4a: arm the drop-guard SYNCHRONOUSLY here — there is NO `.await`
        // between `launch_child` returning `Launched` and this line — then MOVE
        // it into the spawned future. So even a future that is dropped before
        // its first poll (e.g. current-thread runtime: `dispatch().await` then
        // `handle.abort()`) still drops its captured guard → `Drop` fires the
        // `Terminated` cleanup. The whole `[launch → complete]` window is
        // guarded with no gap. The store's four-part CAS makes the cleanup a
        // no-op if the attempt was already settled/superseded.
        let guard = LaunchGuard {
            store: store.clone(),
            fleet_id: fleet_id.clone(),
            task_id: task_id.clone(),
            attempt_id: attempt_id.clone(),
            owner_epoch,
            clock: clock.clone(),
            armed: true,
        };

        let handle = tokio::spawn(async move {
            // `guard` is captured by-move into this future (referenced below),
            // so it is OWNED by the future and dropped with it — including if
            // the future is dropped before its first poll (P1-4a).

            // Per-fleet sem lookup happens INSIDE the guarded future (its await
            // no longer sits between launch and the guard). P2-1: acquire the
            // PER-FLEET permit BEFORE the global one — a task blocked on its
            // fleet's bound must not hold a global slot (head-of-line starving
            // other fleets). Hold BOTH for the whole run.
            let fleet_sem = Self::per_fleet_sem(&per_fleet, &fleet_id, per_fleet_concurrency).await;
            let Ok(_fleet) = fleet_sem.acquire_owned().await else {
                return AttemptOutcome::Aborted {
                    reason: "per-fleet semaphore closed".to_string(),
                };
            };
            let Ok(_global) = global_sem.acquire_owned().await else {
                return AttemptOutcome::Aborted {
                    reason: "global semaphore closed".to_string(),
                };
            };

            let run_clock = clock.clone();
            let outcome = run_attempt(
                store,
                &fleet_id,
                &task_id,
                &attempt_id,
                &task_view,
                &factory,
                &working_dir,
                deadline,
                owner_epoch,
                move || (run_clock)(),
            )
            .await;

            // P1-4b: disarm ONLY when the attempt is settled-or-not-ours (see
            // `should_disarm`). A `RecordError` — our own store CAS
            // (`mark_running`/`complete_child`) hit an infra error, so the
            // attempt may still be live and ours — KEEPS the guard armed so
            // `Drop` un-wedges it.
            if should_disarm(&outcome) {
                guard.disarm();
            }
            outcome
        });

        Ok(Dispatched {
            launch,
            handle: Some(handle),
        })
    }
}

/// Whether an [`AttemptOutcome`] means the [`LaunchGuard`] should be DISARMED
/// (the attempt is settled or provably not ours, so no drop-cleanup is owed):
///
/// - `Completed` / `Superseded` — the attempt reached a durable terminal state.
/// - `Aborted` — `mark_running` returned `Superseded`, a genuine lost race, so
///   the attempt belongs to someone else.
///
/// A [`AttemptOutcome::RecordError`] — our own store CAS
/// (`mark_running`/`complete_child`) hit an infra error, so the attempt may
/// still be live AND ours — must KEEP the guard armed (returns `false`) so its
/// `Drop` best-effort completes it and can't wedge the child in `Launching`
/// (round-4 P1).
fn should_disarm(outcome: &AttemptOutcome) -> bool {
    matches!(
        outcome,
        AttemptOutcome::Completed { .. }
            | AttemptOutcome::Superseded
            | AttemptOutcome::Aborted { .. }
    )
}

/// Drop-guard over the `[launch → complete]` region (P1-4).
///
/// If the in-flight run exits WITHOUT disarming — a panic, a runtime cancel,
/// an early abort (even before the future's first poll, since the guard is
/// captured by the future — P1-4a), or a `RecordError` where our own
/// `complete_child` failed (P1-4b) — it best-effort completes the attempt
/// `Terminated`, so a launched child can never wedge in `Launching`. The
/// store's four-part CAS (fleet + task + attempt + owner-epoch) renders the
/// completion a no-op when the attempt was already settled or superseded.
///
/// **Shutdown residual (P1-4c):** the cleanup is spawned onto the current
/// Tokio runtime via [`tokio::runtime::Handle::try_current`] — so a `Drop`
/// with no runtime (e.g. a synchronous unit test) does not panic. During
/// runtime/process shutdown the in-process cleanup may not run at all (a
/// dropped/never-polled cleanup task, or no runtime). Recovery then falls to
/// BOOT reconciliation: `FleetKernelStore::reconcile` interrupts the stale
/// (foreign-`owner_epoch`) lease on the next boot. That boot-reconcile contract
/// is the owning daemon's (a later wiring PR), not this crate's.
struct LaunchGuard {
    store: Arc<FleetKernelStore>,
    fleet_id: String,
    task_id: String,
    attempt_id: String,
    owner_epoch: u64,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    armed: bool,
}

impl LaunchGuard {
    /// Consume the guard on the normal path so its [`Drop`] is a no-op.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // P1-4c: only spawn if a runtime is in scope. A `Drop` outside any
        // runtime (a sync unit test, or process shutdown) would otherwise
        // PANIC in `tokio::spawn`; here it degrades to boot reconciliation.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                fleet_id = %self.fleet_id, task_id = %self.task_id, attempt_id = %self.attempt_id,
                "fleet worker: drop-guard could not complete the attempt (no runtime); \
                 boot reconciliation will interrupt the stale lease",
            );
            return;
        };
        let store = self.store.clone();
        let fleet_id = std::mem::take(&mut self.fleet_id);
        let task_id = std::mem::take(&mut self.task_id);
        let attempt_id = std::mem::take(&mut self.attempt_id);
        let owner_epoch = self.owner_epoch;
        let now = (self.clock)();
        const INTERRUPTED: &str = "attempt interrupted before completion";
        runtime.spawn(async move {
            // `complete_child` requires the attempt to be `Running`. A pre-poll
            // abort leaves it `Launching` (its `mark_running` never ran), so
            // best-effort advance it first. `mark_running` is a CAS (needs
            // Launching + current attempt): it succeeds for a still-Launching
            // attempt, and harmlessly errors for one already Running (the
            // normal interrupted case) or superseded — either way
            // `complete_child`'s own four-part CAS then settles or no-ops.
            let _ = store.mark_running(&task_id, &attempt_id).await;
            let snapshot = ChildResultSnapshot {
                output: String::new(),
                success: false,
                tokens_used: 0,
                files: Vec::new(),
                error: Some(INTERRUPTED.to_string()),
            };
            if let Err(err) = store
                .complete_child(
                    &fleet_id,
                    &task_id,
                    &attempt_id,
                    AcceptanceVerdict::Terminated {
                        reason: INTERRUPTED.to_string(),
                    },
                    snapshot,
                    0,
                    owner_epoch,
                    now,
                )
                .await
            {
                tracing::warn!(
                    %fleet_id, %task_id, %attempt_id, error = %err,
                    "fleet worker: drop-guard completion failed (advisory; recovery reconciles)",
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use octos_fleet::ChildStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn pool_config(work: &TempDir, global: usize, per_fleet: usize) -> PoolConfig {
        PoolConfig {
            global_concurrency: global,
            per_fleet_concurrency: per_fleet,
            deadline: Duration::from_secs(30),
            owner_epoch: EPOCH,
            lease_ttl_ms: TTL,
            projected_tokens: PROJECTED,
            workspace_root: work.path().to_path_buf(),
        }
    }

    fn fixed_clock() -> Arc<dyn Fn() -> u64 + Send + Sync> {
        Arc::new(|| NOW)
    }

    /// Poll a child's status until it equals `want` (or panic after 5s).
    async fn wait_for_status(
        store: &FleetKernelStore,
        fleet_id: &str,
        task_id: &str,
        want: ChildStatus,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = store
                .get_child(fleet_id, task_id)
                .await
                .unwrap()
                .unwrap()
                .status;
            if status == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child {task_id} did not reach {want:?} within 5s (last: {status:?})",
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Poll a child until it reaches a terminal status (or panic after 5s).
    async fn wait_for_terminal(
        store: &FleetKernelStore,
        fleet_id: &str,
        task_id: &str,
    ) -> ChildStatus {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = store
                .get_child(fleet_id, task_id)
                .await
                .unwrap()
                .unwrap()
                .status;
            if matches!(status, ChildStatus::Succeeded | ChildStatus::Failed) {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child {task_id} did not reach a terminal state within 5s (last: {status:?})",
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn record_error_keeps_guard_armed_others_disarm() {
        // round-4 P1: the outcome→disarm mapping. Settled-or-not-ours outcomes
        // disarm; a store-CAS infra error (`RecordError`) KEEPS the guard armed
        // so its `Drop` un-wedges a possibly-still-live attempt.
        assert!(should_disarm(&AttemptOutcome::Completed {
            verdict: AcceptanceVerdict::Accepted {
                evidence: Vec::new()
            },
        }));
        assert!(should_disarm(&AttemptOutcome::Superseded));
        assert!(should_disarm(&AttemptOutcome::Aborted {
            reason: "mark_running superseded".to_string(),
        }));
        assert!(
            !should_disarm(&AttemptOutcome::RecordError {
                reason: "mark_running errored: redb down".to_string(),
            }),
            "an infra RecordError must keep the guard armed for Drop cleanup",
        );
    }

    #[tokio::test]
    async fn dispatch_double_launch_is_rejected() {
        let (_sd, store) = fresh_store().await;
        create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let first = pool.dispatch("f1", "a").await.unwrap();
        assert!(
            matches!(first.launch, LaunchOutcome::Launched { .. }),
            "first dispatch must launch, got {:?}",
            first.launch,
        );

        // The child is now in flight with a live attempt: a second dispatch
        // of the same task is a double-launch and spawns NO work.
        let second = pool.dispatch("f1", "a").await.unwrap();
        assert_eq!(second.launch, LaunchOutcome::RejectedDoubleLaunch);
        assert!(
            second.handle.is_none(),
            "rejected dispatch must not spawn a run"
        );

        // Let the first attempt finish cleanly.
        let outcome = first.handle.unwrap().await.unwrap();
        assert!(
            matches!(outcome, AttemptOutcome::Completed { .. }),
            "got {outcome:?}",
        );
    }

    #[tokio::test]
    async fn dispatch_preflight_rejects_bad_workspace_root_without_launching() {
        // P1-4a: a workspace_root that is an existing FILE makes the preflight
        // `create_dir_all(<file>/f1/a)` fail BEFORE launch_child — so dispatch
        // errors and NO durable attempt is created: the child stays Ready and
        // is never wedged in `Launching`.
        let (_sd, store) = fresh_store().await;
        create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;

        let tmp = TempDir::new().unwrap();
        let file_root = tmp.path().join("not-a-dir");
        tokio::fs::write(&file_root, b"x").await.unwrap();

        let cfg = PoolConfig {
            global_concurrency: 4,
            per_fleet_concurrency: 4,
            deadline: Duration::from_secs(30),
            owner_epoch: EPOCH,
            lease_ttl_ms: TTL,
            projected_tokens: PROJECTED,
            workspace_root: file_root,
        };
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let pool = FleetWorkerPool::new(store.clone(), Arc::new(factory), cfg, fixed_clock());

        let result = pool.dispatch("f1", "a").await;
        assert!(
            result.is_err(),
            "preflight must reject a file workspace_root, got {result:?}",
        );

        // No durable attempt: the child is still Ready (never Launching).
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(
            child.status,
            ChildStatus::Ready,
            "child must stay Ready after a rejected preflight, not wedge in Launching",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupted_attempt_ends_terminal_not_stuck_launching() {
        // P1-4b: an attempt cancelled mid-run must NOT stay `Launching`/
        // `Running` forever — the drop-guard best-effort completes it
        // `Terminated`, so the child ends terminal (`Failed`).
        let (_sd, store) = fresh_store().await;
        create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;

        let work = TempDir::new().unwrap();
        // Sleeps 30s so the attempt is mid-run when we abort it.
        let (_md, factory) = factory_for(Arc::new(SleepProvider {
            hold: Duration::from_secs(30),
        }))
        .await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        assert!(matches!(d.launch, LaunchOutcome::Launched { .. }));
        let handle = d.handle.unwrap();

        // Wait until the attempt has reached mark_running (child Running) so
        // the interruption lands squarely inside the guarded region.
        wait_for_status(&store, "f1", "a", ChildStatus::Running).await;

        // Cancel the in-flight run mid-sleep: the guard must fire.
        handle.abort();

        // The child must reach a terminal state (Failed), not stay Launching.
        let final_status = wait_for_terminal(&store, "f1", "a").await;
        assert_eq!(
            final_status,
            ChildStatus::Failed,
            "an interrupted attempt must end Failed via the drop-guard",
        );
    }

    #[tokio::test]
    async fn pre_poll_abort_does_not_leak_launching_child() {
        // P1-4a: on a current-thread runtime, `dispatch().await` schedules the
        // run future but does NOT poll it before we `abort()`. Because the
        // guard is armed synchronously at dispatch (no await after launch) and
        // MOVED into the future, dropping the never-polled future still fires
        // the Terminated cleanup — so the child does not wedge in `Launching`.
        let (_sd, store) = fresh_store().await;
        create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        assert!(matches!(d.launch, LaunchOutcome::Launched { .. }));
        let handle = d.handle.unwrap();
        // Abort BEFORE the spawned future is ever polled (no yield since
        // dispatch returned).
        handle.abort();

        // The guard must still drive the child terminal (Failed), not leak it
        // in Launching.
        let status = wait_for_terminal(&store, "f1", "a").await;
        assert_eq!(
            status,
            ChildStatus::Failed,
            "a pre-poll abort must not leak a Launching child",
        );
    }

    #[tokio::test]
    async fn zero_concurrency_is_clamped_not_hung() {
        // P2-1: zero global AND per-fleet concurrency would launch the child
        // then wait forever on a permit that never frees. The pool clamps zero
        // to 1, so the attempt runs to completion.
        let (_sd, store) = fresh_store().await;
        create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 0, 0),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        assert!(matches!(d.launch, LaunchOutcome::Launched { .. }));
        let outcome = tokio::time::timeout(Duration::from_secs(10), d.handle.unwrap())
            .await
            .expect("zero concurrency must not hang (clamped to 1)")
            .unwrap();
        assert!(
            matches!(outcome, AttemptOutcome::Completed { .. }),
            "got {outcome:?}",
        );
    }

    #[tokio::test]
    async fn dispatch_not_ready_task_is_rejected() {
        let (_sd, store) = fresh_store().await;
        // b depends on a, so b is not Ready.
        create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], vec![]), task_spec("b", &["a"], vec![])],
        )
        .await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let dispatched = pool.dispatch("f1", "b").await.unwrap();
        assert_eq!(dispatched.launch, LaunchOutcome::RejectedNotReady);
        assert!(dispatched.handle.is_none());
    }

    #[tokio::test]
    async fn pool_bounds_concurrency() {
        let (_sd, store) = fresh_store().await;
        // Five independent (dep-free) tasks — all Ready at once.
        let tasks = ["t0", "t1", "t2", "t3", "t4"]
            .iter()
            .map(|id| task_spec(id, &[], vec![]))
            .collect();
        create_fleet(store.clone(), "f1", tasks).await;

        let active = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(ConcurrencyProvider {
            active: active.clone(),
            max: max.clone(),
            hold: Duration::from_millis(150),
        });

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(provider).await;
        // global cap 2, per-fleet cap high so the GLOBAL cap is the binding one.
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 2, 5),
            fixed_clock(),
        );

        let mut handles = Vec::new();
        for id in ["t0", "t1", "t2", "t3", "t4"] {
            let d = pool.dispatch("f1", id).await.unwrap();
            assert!(
                matches!(d.launch, LaunchOutcome::Launched { .. }),
                "{id} must launch"
            );
            handles.push(d.handle.unwrap());
        }

        let mut completed = 0;
        for h in handles {
            let outcome = h.await.unwrap();
            assert!(
                matches!(outcome, AttemptOutcome::Completed { .. }),
                "got {outcome:?}"
            );
            completed += 1;
        }

        assert_eq!(completed, 5, "all five attempts must complete");
        let observed = max.load(Ordering::SeqCst);
        assert!(
            observed <= 2,
            "observed max concurrency {observed} exceeded the global bound of 2",
        );
        assert!(
            observed >= 2,
            "expected the pool to actually run 2 attempts in parallel (observed {observed})",
        );
        // All five children reached Succeeded.
        for id in ["t0", "t1", "t2", "t3", "t4"] {
            assert_eq!(
                store.get_child("f1", id).await.unwrap().unwrap().status,
                ChildStatus::Succeeded,
                "{id} must be Succeeded",
            );
        }
    }
}
