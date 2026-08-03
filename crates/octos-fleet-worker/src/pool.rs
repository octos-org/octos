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

use octos_agent::TokenTracker;
use octos_core::safe_filename;
use octos_fleet::{
    AcceptanceVerdict, ChildResultSnapshot, ChildStatus, Fleet, FleetKernelStore, LaunchOutcome,
};

use crate::escalate::EscalationSlot;
use crate::worker::{escalation_tokens_with_floor, tracked_tokens};
use crate::{AgentFactory, AttemptOutcome, WorktreeContext, run_attempt};

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
    /// #1857 PR 5a — the profile this pool is bound to (its `llm` / `memory` /
    /// sandbox come from that profile's runtime). The goal keeper fences
    /// dispatch on it: a goal set on a DIFFERENT profile must not run its tasks
    /// on this profile's model/sandbox while its wake returns to the other
    /// profile. Read back via [`FleetWorkerPool::keeper_profile_id`].
    pub keeper_profile_id: String,
    /// Whether the resolved sandbox backend supports FULL-FS write for a
    /// `FsGrant::Host` worker — the third gate condition for the worktree flow
    /// (§5). Computed at serve boot from the base sandbox's
    /// `supports_repo_git_write()`: `true` for bwrap and unrestricted-read macOS,
    /// `false` for docker, restricted-read macOS, Landlock, AppContainer, and no
    /// sandbox. When `false`, EVERY task takes the SCRATCH fallback even on a git
    /// controller root with a Host grant (running the worktree flow on a backend
    /// that can't grant `.git` read+write would lose the deliverable — the worker
    /// couldn't commit, then the checkout is removed with no branch update). This
    /// is the surviving kernel of the parked `honors_write_allow_paths` gate.
    pub repo_git_write_supported: bool,
}

/// The result of a [`FleetWorkerPool::dispatch`]: the typed launch decision
/// plus, on `Launched`, the background run's [`JoinHandle`]. A `Rejected*`
/// launch carries `handle: None` (no work was spawned).
#[derive(Debug)]
pub struct Dispatched {
    pub launch: LaunchOutcome,
    pub handle: Option<JoinHandle<AttemptOutcome>>,
}

/// Per-(fleet, task) preflight locks — a task's lock serializes concurrent
/// dispatches of it. Aliased to keep the pool field under clippy's
/// `type_complexity` threshold.
type PreflightLocks = Arc<Mutex<HashMap<(String, String), Arc<Mutex<()>>>>>;

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
    /// Per-(fleet, task) preflight locks. Held across [live-check → worktree
    /// prep → launch → spawn] so two concurrent dispatches of the SAME Ready
    /// task cannot both pass the non-atomic live check and race on (remove/
    /// re-add) the same task-stable checkout; the loser sees the winner's live
    /// attempt and `launch_child` double-launch rejects it. Like `per_fleet`,
    /// entries are never pruned (bounded by the number of distinct tasks).
    preflight_locks: PreflightLocks,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

// Manual `Debug` (the `factory` and `clock` fields are not `Debug`): render the
// static [`PoolConfig`] so an owning type — e.g. a keeper orchestrator holding
// an `Option<Arc<FleetWorkerPool>>` — can still `derive(Debug)`.
impl std::fmt::Debug for FleetWorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetWorkerPool")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
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
            preflight_locks: Arc::new(Mutex::new(HashMap::new())),
            clock,
        }
    }

    /// Get-or-create the per-(fleet, task) preflight lock.
    async fn preflight_lock(&self, fleet_id: &str, task_id: &str) -> Arc<Mutex<()>> {
        self.preflight_locks
            .lock()
            .await
            .entry((fleet_id.to_string(), task_id.to_string()))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Roll back a checkout prepared by THIS dispatch when the launch did NOT
    /// reach a durable attempt (a store error, or a NotReady/BudgetExceeded
    /// rejection after a successful prep), so no orphaned checkout is left. Keeps
    /// the branch (resumable / the deliverable).
    async fn rollback_prepared_checkout(cleanup: Option<WorktreeCleanup>) {
        if let Some(wt) = cleanup {
            let _ = tokio::task::spawn_blocking(move || {
                octos_core::remove_checkout_keep_branch(&wt.repo_root, &wt.work_root, &wt.checkout);
            })
            .await;
        }
    }

    /// #1857 PR 5a — the profile this pool is bound to (from [`PoolConfig`]).
    /// The goal keeper compares a goal's profile against this to fence
    /// cross-profile dispatch (a profile-B goal must not run on a profile-A
    /// pool). See [`PoolConfig::keeper_profile_id`].
    pub fn keeper_profile_id(&self) -> &str {
        &self.cfg.keeper_profile_id
    }

    /// #1857 PR 5a — tokens reserved on the fleet budget per launch (soft
    /// admission). The keeper reads it to warn when a goal's whole token budget
    /// can't fund even one task (every launch would be `RejectedBudgetExceeded`).
    pub fn projected_tokens(&self) -> u64 {
        self.cfg.projected_tokens
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
        // Serialize preflight+launch per (fleet, task). Held across [live-check →
        // worktree prep → launch → spawn] (released when dispatch returns, after
        // the run is spawned), so two concurrent dispatches of the SAME Ready
        // task cannot both pass the non-atomic live check and race on the same
        // task-stable checkout; the loser sees the winner's live attempt and is
        // double-launch rejected below.
        let task_lock = self.preflight_lock(fleet_id, task_id).await;
        let _preflight = task_lock.lock().await;

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

        // Snapshot the plan revision the worktree decision + launch are made
        // against, so a concurrent replan that narrows the grant during the (slow)
        // worktree prep is caught by a CAS at launch (codex fix #3, below).
        let snapshot_revision = view.revision;

        // Worktree preflight (before any durable attempt). Run this attempt
        // inside a REAL `git worktree` of the controller repo on the task-stable
        // branch `fleet/<fleet>/<task>` — so the fleet does durable, PARALLEL
        // repository work and a restarted attempt resumes from the dead one's
        // last commit — ONLY when ALL gate conditions hold:
        //   1. a COHERENT FULL-TRUST grant (codex fix #1): the operator granted
        //      BOTH `FsGrant::Host` AND `NetworkGrant::Full`, read per-attempt.
        //      This removes the trust GRADIENT: a `Host-FS + restricted-network`
        //      worker is NOT truly isolated (full FS lets it bridge any network
        //      fence — host `AF_UNIX` sockets survive `--unshare-net`; a planted
        //      local `.git` filter runs controller-side on the next worktree add),
        //      so we require full network too, making both escapes MOOT (a
        //      full-network worker gains nothing by bridging). A network-ISOLATED
        //      worktree worker would need the parked `.git` deny-fence — a
        //      DEFERRED follow-up; for v1 a worktree = a full-permission worker.
        //   2. the fleet is on a git repo (`controller_workspace_root` Some AND
        //      `probe_git_repo` Ok(true));
        //   3. the backend supports repo `.git` write (`repo_git_write_supported`).
        // Any false → the scratch cwd fallback (cwd-confined, no `repo_git_write`).
        let full_trust =
            task_view.grant.fs.is_host() && task_view.grant.network.allows_raw_egress();
        let repo_root: Option<PathBuf> = if full_trust && self.cfg.repo_git_write_supported {
            self.store
                .get_fleet(fleet_id)
                .await?
                .and_then(|rec| rec.controller_workspace_root)
                .map(PathBuf::from)
        } else {
            None
        };

        // Probe with ERROR PROPAGATION — a probe error (git missing / permission
        // / spawn failure) must NOT silently scratch a REAL repo; it returns Err
        // (no durable attempt, child stays Ready). Only a CONFIRMED non-repo (or
        // absent root) takes the scratch fallback.
        let is_repo = match &repo_root {
            Some(root) => {
                let root = root.clone();
                tokio::task::spawn_blocking(move || octos_core::probe_git_repo(&root))
                    .await
                    .map_err(|e| eyre!("preflight: repo probe join failed: {e}"))?
                    .map_err(|e| {
                        eyre!("preflight: repo probe for {fleet_id}/{task_id} failed: {e}")
                    })?
            }
            None => false,
        };

        // The worktree cleanup + auto-commit context, populated only in the
        // worktree case; None for the scratch fallback.
        let mut worktree_cleanup: Option<WorktreeCleanup> = None;
        let mut worktree_ctx: Option<WorktreeContext> = None;

        if is_repo {
            let root = repo_root
                .clone()
                .expect("repo_root is Some when is_repo is true");
            // A live attempt already owns this task's stable checkout/branch
            // (this dispatch will be a double-launch): SKIP the destructive
            // worktree reconcile — it would clobber the live checkout — and let
            // `launch_child` reject below. Reconciliation is otherwise safe
            // because a re-launch only happens AFTER the prior attempt was
            // interrupted (single-writer-per-task). The per-task preflight lock
            // makes this live check + the launch that follows atomic w.r.t. a
            // concurrent same-task dispatch.
            let live = matches!(
                self.store
                    .get_child(fleet_id, task_id)
                    .await?
                    .map(|c| c.status),
                Some(ChildStatus::Launching) | Some(ChildStatus::Running)
            );
            if !live {
                let branch = format!(
                    "fleet/{}/{}",
                    safe_filename(fleet_id),
                    safe_filename(task_id)
                );
                // `prepare_fleet_worktree` shells out to git several times; run
                // it on the blocking pool. A git failure returns Err with NO
                // durable attempt (nothing launched yet).
                let wd = working_dir.clone();
                let work_root = self.cfg.workspace_root.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    octos_core::prepare_fleet_worktree(&root, &work_root, &branch, &wd)
                })
                .await
                .map_err(|e| eyre!("preflight: worktree prep join failed: {e}"))?
                .map_err(|e| {
                    eyre!("preflight: worktree prep for {fleet_id}/{task_id} failed: {e}")
                })?;
                worktree_ctx = Some(WorktreeContext {
                    repo_root: prepared.repo_root.clone(),
                    branch: prepared.branch.clone(),
                    base_commit: prepared.base_commit.clone(),
                    git_dir: prepared.git_dir.clone(),
                });
                worktree_cleanup = Some(WorktreeCleanup {
                    repo_root: prepared.repo_root,
                    work_root: self.cfg.workspace_root.clone(),
                    checkout: prepared.checkout,
                });
            }
        } else {
            // SCRATCH fallback (not Host-granted, no controller root, not a git
            // repo, or a backend that can't grant full-FS write): a fleet keeps
            // working, cwd-only writable.
            tokio::fs::create_dir_all(&working_dir).await.map_err(|e| {
                eyre!(
                    "preflight: create working dir {} failed: {e}",
                    working_dir.display()
                )
            })?;
        }

        // ---- LAUNCH (durable state) ----
        let now = (self.clock)();
        let launch = match self
            .store
            .launch_child(
                fleet_id,
                task_id,
                self.cfg.projected_tokens,
                now,
                self.cfg.owner_epoch,
                self.cfg.lease_ttl_ms,
            )
            .await
        {
            Ok(launch) => launch,
            Err(err) => {
                // A store error AFTER a successful prep would orphan the
                // just-created checkout — roll it back (keep the branch).
                Self::rollback_prepared_checkout(worktree_cleanup.take()).await;
                return Err(err);
            }
        };

        let LaunchOutcome::Launched { attempt_id } = &launch else {
            // RejectedNotReady / RejectedDoubleLaunch / RejectedBudgetExceeded:
            // no work is spawned. If THIS dispatch prepared a checkout (a
            // non-double-launch rejection — a double-launch skipped prep, leaving
            // `worktree_cleanup` None), roll it back so no orphaned checkout is
            // left behind (keep the branch).
            Self::rollback_prepared_checkout(worktree_cleanup.take()).await;
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
        // PR B (codex round-4, defect 1) — the exact reservation `launch_child`
        // just placed (== the attempt's `reserved_tokens`), threaded into both
        // `run_attempt` and the LaunchGuard as the RELIABLE never-zero escalation
        // floor (no fallible store re-fetch at settle time).
        let reserved_tokens = self.cfg.projected_tokens;
        let fleet_id = fleet_id.to_string();
        let task_id = task_id.to_string();

        // PR B — the escalation slot, created SYNCHRONOUSLY here (before the
        // guard, no await) and shared into BOTH `run_attempt` (→ the agent's
        // `escalate` valve) AND the LaunchGuard. So if this run is cancelled
        // AFTER the agent recorded an escalation but BEFORE `run_attempt` settled
        // it, the guard settles ESCALATED (child Blocked), not `Terminated` —
        // a cancel/timeout can never DROP a recorded escalation.
        let escalation_slot: EscalationSlot = Arc::new(std::sync::Mutex::new(None));

        // PR B (codex round-3, defect 1) — the shared token tracker, created here
        // (before the guard, no await) alongside the escalation slot and shared
        // into BOTH `run_attempt` (→ the agent's `run_task_with_tracker`, which
        // folds each response's cumulative spend into it) AND the LaunchGuard. So a
        // cancel that drops this run AFTER the agent recorded an escalation but
        // BEFORE `run_attempt` settled it lets the guard settle the REAL tokens the
        // agent spent (never 0) — a 0-commit would let the fresh post-grant attempt
        // double-spend the budget.
        let tracker: Arc<TokenTracker> = Arc::new(TokenTracker::new());

        // P1-4a: arm the drop-guard SYNCHRONOUSLY here — there is NO `.await`
        // between `launch_child` returning `Launched` and this line — then MOVE
        // it into the spawned future. So even a future that is dropped before
        // its first poll (e.g. current-thread runtime: `dispatch().await` then
        // `handle.abort()`) still drops its captured guard → `Drop` fires the
        // `Terminated` (or `Escalated`) cleanup. The whole `[launch → complete]`
        // window is guarded with no gap. The store's four-part CAS makes the
        // cleanup a no-op if the attempt was already settled/superseded.
        let guard = LaunchGuard {
            store: store.clone(),
            fleet_id: fleet_id.clone(),
            task_id: task_id.clone(),
            attempt_id: attempt_id.clone(),
            owner_epoch,
            clock: clock.clone(),
            escalation: escalation_slot.clone(),
            tracker: tracker.clone(),
            reserved_tokens,
            worktree_cleanup,
            armed: true,
        };

        // codex fix #3 — grant/launch atomicity for a WORKTREE attempt. A
        // concurrent replan during the (slow) worktree prep can NARROW the grant
        // (Host→Workspace / Full→None) and re-ready the child, leaving this
        // dispatch about to run the STALE full-trust `task_view` (→ `repo_git_write`
        // on a now-narrowed grant). The preflight lock serialises same-task
        // DISPATCH but not a replan, so fence the plan revision: if it moved since
        // the snapshot, the grant may have changed under us — drop `guard` (which
        // terminates the just-launched attempt AND rolls back the checkout,
        // keeping the branch) and REJECT so the keeper re-dispatches against the
        // fresh plan. Only worktree attempts are fenced (a scratch attempt is
        // cwd-confined regardless of a grant change, so it needs no fence).
        if worktree_ctx.is_some() {
            let current_revision = Fleet::bind(store.clone(), fleet_id.clone())
                .view()
                .await
                .map(|v| v.revision)
                .unwrap_or(u64::MAX); // an unreadable plan is treated as changed (fail safe)
            if current_revision != snapshot_revision {
                // `guard` drops here → terminates the attempt + frees the checkout.
                return Err(eyre!(
                    "plan revision changed during worktree launch ({snapshot_revision} -> \
                     {current_revision}) for {fleet_id}/{task_id}; re-dispatch against the \
                     fresh grant"
                ));
            }
        }

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
                worktree_ctx.as_ref(),
                deadline,
                owner_epoch,
                escalation_slot,
                tracker,
                reserved_tokens,
                move || (run_clock)(),
            )
            .await;

            // P1-4b: disarm ONLY when the attempt is settled-or-not-ours (see
            // `should_disarm`). A `RecordError` — our own store CAS
            // (`mark_running`/`complete_child`) hit an infra error, so the
            // attempt may still be live and ours — KEEPS the guard armed so
            // `Drop` un-wedges it (and, for a worktree attempt, frees its
            // checkout).
            if should_disarm(&outcome) {
                let worktree = guard.disarm();
                // COMPLETION cleanup: on a genuinely `Completed` attempt, remove
                // the worktree checkout but KEEP the `fleet/*` branch — the
                // branch is the deliverable (the deliverable auto-commit already
                // landed inside `run_attempt`). On `Superseded`/`Aborted` the
                // attempt is NOT ours (a newer attempt owns the task-stable
                // checkout), so leave it untouched.
                if matches!(outcome, AttemptOutcome::Completed { .. }) {
                    if let Some(wt) = worktree {
                        let _ = tokio::task::spawn_blocking(move || {
                            octos_core::remove_checkout_keep_branch(
                                &wt.repo_root,
                                &wt.work_root,
                                &wt.checkout,
                            );
                        })
                        .await;
                    }
                }
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
/// - `Escalated` (PR B) — `record_escalation` settled the attempt
///   NON-terminally (child `Blocked`); it is SETTLED (its lease released, its
///   tokens committed), so the guard must DISARM — a `Drop` `Terminated`
///   completion would double-settle (and would clobber the Blocked child).
///
/// A [`AttemptOutcome::RecordError`] — our own store CAS
/// (`mark_running`/`complete_child`/`record_escalation`) hit an infra error, so
/// the attempt may still be live AND ours — must KEEP the guard armed (returns
/// `false`) so its `Drop` best-effort completes it and can't wedge the child in
/// `Launching` (round-4 P1).
fn should_disarm(outcome: &AttemptOutcome) -> bool {
    matches!(
        outcome,
        AttemptOutcome::Completed { .. }
            | AttemptOutcome::Superseded
            | AttemptOutcome::Aborted { .. }
            | AttemptOutcome::Escalated { .. }
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
    /// PR B — the shared escalation slot. On a cancel-drop, if the agent already
    /// recorded an escalation here, the guard settles ESCALATED (child Blocked),
    /// not `Terminated` — so a cancel/timeout can never drop a recorded request.
    escalation: EscalationSlot,
    /// PR B (codex round-3, defect 1) — the SAME token tracker `run_attempt`
    /// handed the agent, so a cancel-drop escalation settle commits the REAL
    /// spend (with the attempt's reserved tokens as a never-zero floor), not 0.
    tracker: Arc<TokenTracker>,
    /// PR B (codex round-4, defect 1) — the attempt's reservation
    /// (`PoolConfig::projected_tokens`), the RELIABLE never-zero escalation floor
    /// on a cancel-drop settle (no fallible store re-fetch).
    reserved_tokens: u64,
    /// The worktree cleanup context when the attempt runs in a git worktree;
    /// `None` for the scratch fallback. On an interrupted (armed) drop, the
    /// checkout is best-effort removed with its `fleet/*` branch KEPT, so a dead
    /// attempt doesn't leave a (locked) checkout blocking the next attempt's
    /// re-add (and the branch stays as resumable progress). The containment
    /// `work_root` guards the removal from escaping `fleet-work`.
    worktree_cleanup: Option<WorktreeCleanup>,
    armed: bool,
}

/// The context needed to free a fleet worktree CHECKOUT while keeping its
/// branch: the canonical repo root, the `fleet-work` containment root (a removal
/// is refused if the checkout is not provably under it), and the checkout path.
/// Carried by [`LaunchGuard`] (interrupt cleanup), returned by
/// [`LaunchGuard::disarm`] (completion cleanup), and used for the prep→launch
/// rollback.
#[derive(Debug, Clone)]
struct WorktreeCleanup {
    repo_root: PathBuf,
    work_root: PathBuf,
    checkout: PathBuf,
}

impl LaunchGuard {
    /// Consume the guard on the normal path so its [`Drop`] is a no-op. Returns
    /// the [`WorktreeCleanup`] (if any) so the caller can run the COMPLETION
    /// checkout cleanup itself (remove the checkout, keep the branch) on a
    /// genuinely completed attempt.
    fn disarm(mut self) -> Option<WorktreeCleanup> {
        self.armed = false;
        self.worktree_cleanup.take()
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
        let tracker = self.tracker.clone();
        let reserved_tokens = self.reserved_tokens;
        let fleet_id = std::mem::take(&mut self.fleet_id);
        let task_id = std::mem::take(&mut self.task_id);
        let attempt_id = std::mem::take(&mut self.attempt_id);
        let owner_epoch = self.owner_epoch;
        let worktree = self.worktree_cleanup.take();
        let now = (self.clock)();
        // PR B — if the agent recorded an escalation before this run was
        // cancelled, PRECEDENCE goes to the escalation: settle NON-terminally
        // (child Blocked), never `Terminated`. Take it out of the slot so the
        // decision is made synchronously here (no await between the check and
        // the spawn). `record_escalation`'s own four-part CAS no-ops if
        // `run_attempt` already committed it.
        let pending = self
            .escalation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        const INTERRUPTED: &str = "attempt interrupted before completion";
        runtime.spawn(async move {
            // `complete_child`/`record_escalation` both require the attempt to be
            // `Running`. A pre-poll abort leaves it `Launching` (its
            // `mark_running` never ran), so best-effort advance it first.
            // `mark_running` is a CAS (needs Launching + current attempt): it
            // succeeds for a still-Launching attempt, and harmlessly errors for
            // one already Running (the normal interrupted case) or superseded —
            // either way the settle op's own four-part CAS then settles or no-ops.
            let _ = store.mark_running(&task_id, &attempt_id).await;
            if let Some(request) = pending {
                // Cancelled AFTER an escalation was recorded. Settle the REAL
                // tokens the agent spent — read live from the SAME shared tracker
                // `run_attempt` handed the agent (an atomic, always reliable) —
                // with the attempt's RESERVED tokens (threaded in reliably from the
                // pool config, NOT re-fetched) as a never-zero floor, exactly like
                // the normal `run_attempt` escalation path. It is impossible to
                // settle 0 while real tokens were used: a 0-commit here would
                // release the reservation while committing nothing, letting the
                // fresh post-grant attempt double-spend the budget.
                let tracked = tracked_tokens(&tracker);
                let tokens = escalation_tokens_with_floor(tracked, reserved_tokens);
                if let Err(err) = store
                    .record_escalation(
                        &fleet_id,
                        &task_id,
                        &attempt_id,
                        request,
                        tokens,
                        owner_epoch,
                        now,
                    )
                    .await
                {
                    tracing::warn!(
                        %fleet_id, %task_id, %attempt_id, error = %err,
                        "fleet worker: drop-guard escalation settle failed (advisory; recovery reconciles)",
                    );
                }
                return;
            }
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
            // Best-effort: free the interrupted attempt's worktree checkout,
            // KEEPING the `fleet/*` branch (the deliverable / resumable
            // progress), so a dead attempt doesn't leave a locked checkout
            // blocking the next attempt's re-add.
            if let Some(wt) = worktree {
                let _ = tokio::task::spawn_blocking(move || {
                    octos_core::remove_checkout_keep_branch(
                        &wt.repo_root,
                        &wt.work_root,
                        &wt.checkout,
                    );
                })
                .await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
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
            keeper_profile_id: "test-keeper".to_owned(),
            // The tests thread a MarkerSandbox (a real-isolating test double), so
            // the worktree flow is supported; a #6-specific test overrides this.
            repo_git_write_supported: true,
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
            keeper_profile_id: "test-keeper".to_owned(),
            repo_git_write_supported: true,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn escalation_survives_a_cancel_via_the_guard() {
        // codex round-2 (defect 1): if the run is CANCELLED after the agent
        // recorded an escalation but before `run_attempt` settled it, the
        // LaunchGuard must settle ESCALATED (child Blocked), NOT `Terminated` —
        // a cancel/timeout can never DROP a recorded escalation.
        let (_sd, store) = fresh_store().await;
        create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;

        let work = TempDir::new().unwrap();
        // Escalate on turn 1, then sleep 30s (turn 2) so the slot is SET and the
        // run is still alive when we abort it.
        let (_md, factory) = factory_for(Arc::new(EscalateProvider::then_sleeping(
            "blocked past the cancel",
            Duration::from_secs(30),
        )))
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

        // Wait until the child is Running (the agent turn started), then give
        // turn 1 + the escalate tool time to SET the slot.
        wait_for_status(&store, "f1", "a", ChildStatus::Running).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Cancel mid-run: the guard fires and must settle ESCALATED.
        handle.abort();

        // The child must reach Blocked (via the guard's escalation settle), and
        // must NEVER be dropped to Failed (a Terminated completion).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = store.get_child("f1", "a").await.unwrap().unwrap().status;
            assert_ne!(
                status,
                ChildStatus::Failed,
                "a cancel dropped the escalation to a Terminated/Failed completion",
            );
            if status == ChildStatus::Blocked {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child never reached Blocked (last: {status:?})",
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert!(
            child.pending_escalation.is_some(),
            "the recorded escalation request is preserved through the cancel",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn escalation_cancel_drop_commits_real_tokens_not_zero() {
        // codex round-3 (defect 1): the LaunchGuard's cancel-drop escalation
        // settle must commit the REAL tokens the yielded attempt spent — read from
        // the SAME shared tracker the agent updated, with the reserved tokens as a
        // never-zero floor — NOT 0. A 0-commit would release the reservation while
        // committing nothing, so after the keeper grants a wider grant the FRESH
        // attempt could re-reserve and spend the same budget twice (double-spend).
        let (_sd, store) = fresh_store().await;
        create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;

        let work = TempDir::new().unwrap();
        // Escalate on turn 1 (so the tracker records that turn's real tokens),
        // then sleep 30s so the slot is SET and the run is still alive when we
        // abort it INSIDE the guarded region.
        let (_md, factory) = factory_for(Arc::new(EscalateProvider::then_sleeping(
            "blocked past the cancel",
            Duration::from_secs(30),
        )))
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

        // Wait until Running, then give turn 1 + the escalate tool time to SET the
        // slot AND fold that turn's tokens into the shared tracker.
        wait_for_status(&store, "f1", "a", ChildStatus::Running).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Cancel mid-run: the guard fires and must settle ESCALATED with REAL
        // tokens (not 0).
        handle.abort();

        // Wait for the guard's escalation settle to land (child Blocked).
        wait_for_status(&store, "f1", "a", ChildStatus::Blocked).await;

        // Budget honesty: the cancel-drop settle committed the REAL tokens the
        // yielded attempt spent (> 0), and released its reservation — so the
        // fleet's committed budget already reflects the spend before any re-run.
        let fleet_rec = store.get_fleet("f1").await.unwrap().unwrap();
        assert!(
            fleet_rec.budget.tokens_committed > 0,
            "the cancel-drop escalation settle must commit REAL tokens, never 0 \
             (committed={})",
            fleet_rec.budget.tokens_committed,
        );
        assert_eq!(
            fleet_rec.budget.tokens_reserved, 0,
            "the yielded attempt's reservation is released on the cancel-drop settle",
        );
        // The child carries the pending request (it truly escalated, not Terminated).
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert!(child.pending_escalation.is_some());
        assert_eq!(
            child.tokens_committed, fleet_rec.budget.tokens_committed,
            "the child's committed tokens mirror the fleet settle",
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

    // ---- PR C: worktree workers on the operator's FsGrant::Host ----

    #[tokio::test]
    async fn dispatch_runs_worker_in_git_worktree_and_keeps_branch() {
        // A `FsGrant::Host` task on a git controller root: the pool runs the
        // worker inside a `git worktree` on branch `fleet/f1/a`, the worker
        // commits real work, and on completion the CHECKOUT is removed while the
        // BRANCH (the deliverable) survives with the commit.
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return; // git unavailable
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted(
                "a",
                &[],
                file_exists("out.txt"),
                host_grant(),
            )],
        )
        .await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(GitCommitProvider::new("out.txt"))).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        assert!(
            matches!(d.launch, LaunchOutcome::Launched { .. }),
            "got {:?}",
            d.launch
        );
        let outcome = d.handle.unwrap().await.unwrap();
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "got {outcome:?}",
        );

        // The fleet branch exists AND carries the worker's commit (survives the
        // checkout removal — proof the work is durable on the branch).
        assert!(
            git_ref_exists(repo.path(), "refs/heads/fleet/f1/a"),
            "fleet branch must exist as the deliverable",
        );
        assert!(
            git_branch_contains_file(repo.path(), "fleet/f1/a", "out.txt"),
            "the worker's commit must land on the fleet branch",
        );
        // The worktree checkout (worker cwd) was removed on completion.
        let checkout = work.path().join("f1").join("a");
        assert!(
            !checkout.exists(),
            "checkout must be removed after completion (branch kept)",
        );
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Succeeded,
        );
    }

    #[tokio::test]
    async fn dispatch_reconciles_preexisting_worktree_on_relaunch() {
        // A dead attempt left the branch + checkout behind (task-stable). A
        // re-launch must RECONCILE them (remove + re-add off the branch) rather
        // than fail on a colliding `worktree add -b`, and the worker runs.
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted(
                "a",
                &[],
                file_exists("out.txt"),
                host_grant(),
            )],
        )
        .await;

        let work = TempDir::new().unwrap();
        let checkout = work.path().join("f1").join("a");
        // Simulate the leftover of a dead attempt: branch + checkout present.
        octos_core::prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("pre-create a leftover worktree");
        assert!(checkout.exists(), "leftover checkout present");

        let (_md, factory) = factory_for(Arc::new(GitCommitProvider::new("out.txt"))).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        assert!(
            matches!(d.launch, LaunchOutcome::Launched { .. }),
            "reconcile must not block launch, got {:?}",
            d.launch,
        );
        let outcome = d.handle.unwrap().await.unwrap();
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "got {outcome:?}",
        );
        assert!(git_ref_exists(repo.path(), "refs/heads/fleet/f1/a"));
        assert!(
            git_branch_contains_file(repo.path(), "fleet/f1/a", "out.txt"),
            "the worker commit must land after reconcile",
        );
        assert!(!checkout.exists(), "checkout removed on completion");
    }

    #[tokio::test]
    async fn dispatch_falls_back_to_scratch_when_root_is_not_a_git_repo() {
        // §5 gate condition 2: a Host-granted task on a controller root that
        // exists but is NOT a git repo must use the scratch cwd (today's
        // behaviour): no worktree, no extra write path.
        let (_sd, store) = fresh_store().await;
        let not_repo = TempDir::new().unwrap();
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(not_repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted(
                "a",
                &[],
                file_exists("out.txt"),
                host_grant(),
            )],
        )
        .await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(WriteFileProvider::new("out.txt"))).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        assert!(matches!(d.launch, LaunchOutcome::Launched { .. }));
        let outcome = d.handle.unwrap().await.unwrap();
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "got {outcome:?}",
        );
        // Scratch cwd used (plain dir, kept — not a worktree, not removed).
        let cwd = work.path().join("f1").join("a");
        assert!(
            cwd.join("out.txt").exists(),
            "scratch worker must write into the plain cwd (kept after completion)",
        );
        assert!(
            !not_repo.path().join(".git").exists(),
            "the non-repo root must not have been turned into a repo",
        );
    }

    #[tokio::test]
    async fn dispatch_falls_back_to_scratch_when_task_not_host_granted() {
        // §5 gate condition 1: a git controller root but a task granted only the
        // default `FsGrant::Workspace` runs in SCRATCH — no worktree, no fleet
        // branch — because the worktree flow requires the OPERATOR's
        // `FsGrant::Host`.
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec("a", &[], file_exists("out.txt"))], // minimal → Workspace
        )
        .await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(WriteFileProvider::new("out.txt"))).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        let outcome = d.handle.unwrap().await.unwrap();
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "got {outcome:?}",
        );
        assert!(
            work.path().join("f1").join("a").join("out.txt").exists(),
            "a Workspace-granted worker must write into the scratch cwd",
        );
        assert!(
            !git_ref_exists(repo.path(), "refs/heads/fleet/f1/a"),
            "a Workspace-granted task must NOT create a fleet worktree branch",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupted_worktree_attempt_removes_checkout_keeps_branch() {
        // An interrupted worktree attempt: the LaunchGuard settles it terminal
        // (child Failed) AND best-effort removes the checkout, KEEPING the
        // fleet branch (so a dead attempt doesn't leave a locked checkout).
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted("a", &[], vec![], host_grant())],
        )
        .await;

        let work = TempDir::new().unwrap();
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
        wait_for_status(&store, "f1", "a", ChildStatus::Running).await;
        let checkout = work.path().join("f1").join("a");
        assert!(checkout.exists(), "checkout present while the attempt runs");

        handle.abort();

        let final_status = wait_for_terminal(&store, "f1", "a").await;
        assert_eq!(
            final_status,
            ChildStatus::Failed,
            "an interrupted attempt must end Failed via the drop-guard",
        );
        // The checkout removal runs in the guard's detached cleanup task: poll.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while checkout.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !checkout.exists(),
            "the interrupted attempt must remove its worktree checkout",
        );
        assert!(
            git_ref_exists(repo.path(), "refs/heads/fleet/f1/a"),
            "the fleet branch must survive the interrupt (deliverable / resumable)",
        );
    }

    #[tokio::test]
    async fn worktree_deliverable_autocommitted_even_without_worker_commit() {
        // A worker WRITES a file but does NOT commit. After completion the BRANCH
        // must contain the file (the sandboxed auto-commit landed it) and the
        // checkout is removed — the deliverable is never lost.
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted(
                "a",
                &[],
                file_exists("out.txt"),
                host_grant(),
            )],
        )
        .await;

        let work = TempDir::new().unwrap();
        // WriteFileProvider writes out.txt via the native write_file tool and does
        // NOT git-commit — the sandboxed auto-commit must land it on the branch.
        let (_md, factory) = factory_for(Arc::new(WriteFileProvider::new("out.txt"))).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        let outcome = d.handle.unwrap().await.unwrap();
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "got {outcome:?}",
        );
        assert!(
            git_branch_contains_file(repo.path(), "fleet/f1/a", "out.txt"),
            "the auto-commit must land the (uncommitted) worker file on the branch",
        );
        let checkout = work.path().join("f1").join("a");
        assert!(!checkout.exists(), "checkout removed after completion");
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Succeeded,
        );
    }

    #[tokio::test]
    async fn worktree_empty_branch_is_not_marked_succeeded() {
        // A worktree task whose worker produced NOTHING must not be recorded
        // Succeeded on an EMPTY branch. With no acceptance criteria the gate would
        // pass on the run's own success, but the empty-branch check downgrades it
        // to Rejected → child Failed.
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted("a", &[], vec![], host_grant())],
        )
        .await;

        let work = TempDir::new().unwrap();
        // SuccessProvider ends the turn writing NOTHING → empty branch.
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        let outcome = d.handle.unwrap().await.unwrap();
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Rejected { .. }
                }
            ),
            "an accepted-but-empty worktree run must be rejected, got {outcome:?}",
        );
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Failed,
            "an empty-branch worktree task must not be marked Succeeded",
        );
    }

    #[tokio::test]
    async fn scratch_fallback_when_backend_cannot_support_full_fs_write() {
        // §5 gate condition 3: a git controller root + a Host grant but a backend
        // that CANNOT grant full-FS write → SCRATCH workspace (no worktree, no
        // fleet branch), and the deliverable still lands in the scratch cwd.
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted(
                "a",
                &[],
                file_exists("out.txt"),
                host_grant(),
            )],
        )
        .await;

        let work = TempDir::new().unwrap();
        let mut cfg = pool_config(&work, 4, 4);
        cfg.repo_git_write_supported = false; // backend cannot grant full-FS write
        let (_md, factory) = factory_for(Arc::new(WriteFileProvider::new("out.txt"))).await;
        let pool = FleetWorkerPool::new(store.clone(), Arc::new(factory), cfg, fixed_clock());

        let d = pool.dispatch("f1", "a").await.unwrap();
        let outcome = d.handle.unwrap().await.unwrap();
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "the fleet still runs (in scratch), got {outcome:?}",
        );
        assert!(
            work.path().join("f1").join("a").join("out.txt").exists(),
            "deliverable must land in the scratch cwd (not lost)",
        );
        assert!(
            !git_ref_exists(repo.path(), "refs/heads/fleet/f1/a"),
            "a non-supporting backend must NOT create a fleet worktree branch",
        );
    }

    #[tokio::test]
    async fn rejected_after_prep_leaves_no_orphaned_checkout() {
        // A task that is PREPPED (worktree created) but then REJECTED by launch
        // (NotReady — an unmet dep) must leave NO orphaned checkout — the
        // prep→launch rollback removes it (keeping the branch).
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        // b depends on a → b is Planned (not Ready): launch_child rejects it, but
        // the pool preps its worktree first.
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![
                task_spec_granted("a", &[], vec![], host_grant()),
                task_spec_granted("b", &["a"], vec![], host_grant()),
            ],
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

        let d = pool.dispatch("f1", "b").await.unwrap();
        assert_eq!(d.launch, LaunchOutcome::RejectedNotReady);
        assert!(d.handle.is_none());
        let checkout = work.path().join("f1").join("b");
        assert!(
            !checkout.exists(),
            "a rejected-after-prep dispatch must roll back the orphaned checkout",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_dispatch_of_one_task_does_not_corrupt_the_worktree() {
        // Two concurrent dispatches of the SAME task must be serialized by the
        // per-task preflight lock — exactly one launches, and the deliverable
        // lands intact (a raced double prep/reconcile would corrupt the shared
        // checkout).
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted(
                "a",
                &[],
                file_exists("out.txt"),
                host_grant(),
            )],
        )
        .await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(GitCommitProvider::new("out.txt"))).await;
        let pool = Arc::new(FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        ));

        let p1 = pool.clone();
        let p2 = pool.clone();
        let (r1, r2) = tokio::join!(
            async move { p1.dispatch("f1", "a").await.unwrap() },
            async move { p2.dispatch("f1", "a").await.unwrap() },
        );
        let launched = [&r1, &r2]
            .iter()
            .filter(|d| matches!(d.launch, LaunchOutcome::Launched { .. }))
            .count();
        assert_eq!(
            launched, 1,
            "exactly one concurrent dispatch may launch (got r1={:?}, r2={:?})",
            r1.launch, r2.launch,
        );
        for d in [r1, r2] {
            if let Some(h) = d.handle {
                let _ = h.await;
            }
        }
        assert!(
            git_ref_exists(repo.path(), "refs/heads/fleet/f1/a"),
            "the fleet branch must exist after concurrent dispatch",
        );
        assert!(
            git_branch_contains_file(repo.path(), "fleet/f1/a", "out.txt"),
            "the deliverable must land intact despite the concurrent dispatch",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn escalation_to_host_upgrades_next_attempt_to_a_worktree_worker() {
        // §6 (PR B interaction): the worktree decision is a pure per-attempt
        // function of the CURRENT grant. A task dispatched under a Workspace grant
        // runs in SCRATCH (no fleet branch); after it escalates and the keeper
        // widens it to `FsGrant::Host` (PlanEdit::SetGrant), the NEXT dispatch
        // re-reads the view, sees `fs.is_host()`, and runs in a WORKTREE — no
        // extra wiring.
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let fleet = create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec("a", &[], file_exists("out.txt"))], // minimal → Workspace
        )
        .await;

        let work = TempDir::new().unwrap();

        // Attempt 1 under the Workspace grant: the worker escalates → child
        // Blocked. It ran in SCRATCH (no fleet branch created).
        let (_me, esc_factory) = factory_for(Arc::new(EscalateProvider::new("need host fs"))).await;
        let esc_pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(esc_factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );
        let d = esc_pool.dispatch("f1", "a").await.unwrap();
        let _ = d.handle.unwrap().await;
        wait_for_status(&store, "f1", "a", ChildStatus::Blocked).await;
        assert!(
            !git_ref_exists(repo.path(), "refs/heads/fleet/f1/a"),
            "the Workspace-granted (scratch) attempt must NOT create a fleet branch",
        );

        // The keeper widens the grant to Host (SetGrant on the Blocked task → Ready).
        let rev = fleet.view().await.unwrap().revision;
        let out = fleet
            .apply_edit(
                octos_fleet::PlanEdit::SetGrant {
                    task_id: "a".into(),
                    grant: host_grant(),
                },
                rev,
                NOW,
            )
            .await
            .unwrap();
        assert!(
            matches!(out, octos_fleet::PlanMutateOutcome::Mutated { .. }),
            "SetGrant must apply to the Blocked task, got {out:?}",
        );

        // Attempt 2 re-reads the view: Host grant → WORKTREE worker committing
        // real work on the fleet branch.
        let (_mc, commit_factory) = factory_for(Arc::new(GitCommitProvider::new("out.txt"))).await;
        let commit_pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(commit_factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );
        let d = commit_pool.dispatch("f1", "a").await.unwrap();
        assert!(
            matches!(d.launch, LaunchOutcome::Launched { .. }),
            "the re-dispatch must launch, got {:?}",
            d.launch,
        );
        let outcome = d.handle.unwrap().await.unwrap();
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "the upgraded attempt must complete accepted, got {outcome:?}",
        );
        assert!(
            git_ref_exists(repo.path(), "refs/heads/fleet/f1/a"),
            "the Host-upgraded attempt must run in a worktree and create the fleet branch",
        );
        assert!(
            git_branch_contains_file(repo.path(), "fleet/f1/a", "out.txt"),
            "the worker's commit must land on the fleet branch",
        );
    }

    // ---- codex HIGH fixes ----

    #[tokio::test]
    async fn dispatch_falls_back_to_scratch_when_network_not_full() {
        // codex fix #1: the worktree path requires a COHERENT full-trust grant —
        // `FsGrant::Host` AND `NetworkGrant::Full`. A MIXED grant (Host FS but
        // `NetworkGrant::None`) is NOT truly isolated, so it must take the SCRATCH
        // fallback (no worktree, no fleet branch, no `repo_git_write`).
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted(
                "a",
                &[],
                file_exists("out.txt"),
                host_fs_no_network_grant(),
            )],
        )
        .await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(WriteFileProvider::new("out.txt"))).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        let outcome = d.handle.unwrap().await.unwrap();
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "the fleet still runs in scratch, got {outcome:?}",
        );
        assert!(
            work.path().join("f1").join("a").join("out.txt").exists(),
            "a Host-FS-but-no-network worker must run in scratch cwd",
        );
        assert!(
            !git_ref_exists(repo.path(), "refs/heads/fleet/f1/a"),
            "a non-full-network grant must NOT create a fleet worktree branch",
        );
    }

    #[tokio::test]
    async fn worktree_terminated_when_backend_lacks_full_fs_write() {
        // codex fix #2b: boot-guarantee ≠ runtime-guarantee. The pool's boot
        // `repo_git_write_supported` is true (so a worktree is allocated), but the
        // RESOLVED per-attempt sandbox reports it CANNOT grant full-FS write (the
        // backend degraded). `run_attempt` must verify this per-attempt and
        // TERMINATE rather than run a worktree worker whose git ops would fail.
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted("a", &[], vec![], host_grant())],
        )
        .await;

        let work = TempDir::new().unwrap();
        // RestrictedSandbox: non-noop but supports_repo_git_write() == false.
        let (_md, factory) =
            factory_for_with(Arc::new(SuccessProvider), restricted_sandbox_factory()).await;
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(factory),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        let d = pool.dispatch("f1", "a").await.unwrap();
        assert!(
            matches!(d.launch, LaunchOutcome::Launched { .. }),
            "the worktree is allocated (boot said supported), got {:?}",
            d.launch,
        );
        let outcome = d.handle.unwrap().await.unwrap();
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Terminated { .. }
                }
            ),
            "a worktree attempt under a non-full-FS-write backend must terminate, got {outcome:?}",
        );
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Failed,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hung_deliverable_commit_is_killed_at_deadline() {
        // codex fix #4: the sandboxed deliverable commit is BOUNDED. A worker
        // plants a `filter.hang.clean = sleep 3000` so the settle `git add -A`
        // hangs; the bounded commit must KILL it at the deadline and terminate the
        // attempt, not hang forever retaining permits. The whole test must finish
        // well under the 3000s the filter would otherwise sleep.
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted("a", &[], vec![], host_grant())],
        )
        .await;

        let work = TempDir::new().unwrap();
        let mut cfg = pool_config(&work, 4, 4);
        cfg.deadline = Duration::from_secs(3); // bounds the agent run AND the commit
        let (_md, factory) = factory_for(Arc::new(HangCleanFilterProvider::new())).await;
        let pool = FleetWorkerPool::new(store.clone(), Arc::new(factory), cfg, fixed_clock());

        let started = std::time::Instant::now();
        let d = pool.dispatch("f1", "a").await.unwrap();
        let outcome = d.handle.unwrap().await.unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(60),
            "the hung commit must be killed at the deadline, not sleep 3000s (took {elapsed:?})",
        );
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Terminated { .. }
                }
            ),
            "a hung deliverable commit must terminate the attempt, got {outcome:?}",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn grant_narrowed_during_launch_is_rejected() {
        // codex fix #3: a concurrent replan that bumps the plan revision during
        // the (slow) worktree prep must be caught by the revision CAS at launch —
        // the dispatch REJECTS (rolling back the checkout) rather than run the
        // stale full-trust grant. Deterministic: a `Retitle` bumper (spec-only,
        // bumps only the revision) runs CONTINUOUSLY for the whole dispatch, so a
        // bump is guaranteed to land between the dispatch's snapshot read and its
        // CAS re-read (prep >> the 1ms bump interval) regardless of alignment.
        let (_sd, store) = fresh_store().await;
        let repo = TempDir::new().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        create_fleet_with_root(
            store.clone(),
            "f1",
            Some(repo.path().to_string_lossy().into_owned()),
            vec![task_spec_granted("a", &[], vec![], host_grant())],
        )
        .await;

        let work = TempDir::new().unwrap();
        let pool = FleetWorkerPool::new(
            store.clone(),
            Arc::new(
                factory_for(Arc::new(GitCommitProvider::new("out.txt")))
                    .await
                    .1,
            ),
            pool_config(&work, 4, 4),
            fixed_clock(),
        );

        // Continuously bump the plan revision (each bump uses the CURRENT
        // revision) for the duration of the dispatch.
        let bump_store = store.clone();
        let bumper = tokio::spawn(async move {
            for _ in 0..2000 {
                if let Ok(v) = Fleet::bind(bump_store.clone(), "f1".to_string())
                    .view()
                    .await
                {
                    let _ = Fleet::bind(bump_store.clone(), "f1".to_string())
                        .apply_edit(
                            octos_fleet::PlanEdit::Retitle {
                                task_id: "a".into(),
                                title: "narrowed".into(),
                                detail: "concurrent replan".into(),
                            },
                            v.revision,
                            NOW,
                        )
                        .await;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        let result = pool.dispatch("f1", "a").await;
        bumper.abort();

        let err =
            result.expect_err("the revision CAS must reject a dispatch racing a concurrent replan");
        assert!(
            err.to_string().contains("revision changed"),
            "the CAS rejection must name the revision change, got: {err}",
        );
        // The rolled-back checkout must not be orphaned (the guard drop frees it).
        let checkout = work.path().join("f1").join("a");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while checkout.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !checkout.exists(),
            "a revision-CAS rejection must roll back the checkout",
        );
    }
}
