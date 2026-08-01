//! `Fleet` — the ergonomic plan-management API over [`FleetKernelStore`].
//!
//! PR 1 shipped the store: individual, revision-/generation-/lease-fenced
//! CAS ops (`create_fleet`, `create_plan`, `add_child`, `launch_child`,
//! `complete_child`, `replan`, …). This module composes those primitives
//! into whole-plan operations a keeper reasons over — **without changing
//! the store's CAS semantics**. It is still standalone: no LLM, no
//! `octos-agent`, unit-testable against a tempdir redb.
//!
//! ## One source of truth per fact (the PR-2 reconciliation)
//!
//! [`PlanTask`] is the **spec** (`task_id`, `title`, `detail`, `deps`,
//! `acceptance`); a task's **live state** is its child's
//! ([`FleetChildRecord::status`] + `current_attempt_id`) and its
//! **outcome + evidence** live with the child's
//! [`FleetChildRecord::outcome`] (the [`AcceptanceVerdict`]). [`Fleet::view`]
//! *joins* the two back together for rendering; it never persists a second
//! copy of the live state.
//!
//! ## What `Fleet` deliberately does NOT wrap
//!
//! `launch_child` / `mark_running` (the executor's job in PR 3) are left on
//! the store and reached via [`Fleet::store`]. `Fleet` owns *plan
//! management* — create, view, readiness, edits, outcome recording — not
//! attempt execution.

use std::collections::HashMap;
use std::sync::Arc;

use eyre::Result;
use octos_core::SessionKey;

use crate::grant::WorkerGrant;
use crate::records::{
    AcceptanceCriterion, AcceptanceVerdict, ChildResultSnapshot, ChildStatus, DecisionKind,
    DurablePlan, EscalationRequest, EvidenceRef, FleetBudget, FleetChildRecord, FleetStatus,
    PlanTask, SCHEMA_VERSION,
};
use crate::store::{CompleteOutcome, FleetKernelStore, PlanMutateOutcome};

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// The spec for one task handed to [`Fleet::create`] or
/// [`PlanEdit::AddTask`] / [`PlanEdit::SplitTask`]. Structurally the
/// author-facing form of a [`PlanTask`] (an input DTO): it carries *only*
/// spec fields, never execution state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub task_id: String,
    pub title: String,
    pub detail: String,
    /// task_ids that must be `Succeeded` before this task is launchable.
    pub deps: Vec<String>,
    pub acceptance: Vec<AcceptanceCriterion>,
    /// PR A — the operator grant the master provisions this task's worker with
    /// (network / tools / filesystem). Defaults to [`WorkerGrant::minimal`]
    /// (today's closed worker) when the master specifies nothing.
    pub grant: WorkerGrant,
}

impl From<TaskSpec> for PlanTask {
    fn from(s: TaskSpec) -> Self {
        PlanTask {
            task_id: s.task_id,
            title: s.title,
            detail: s.detail,
            deps: s.deps,
            acceptance: s.acceptance,
            grant: s.grant,
        }
    }
}

/// A high-level plan edit (see [`Fleet::apply_edit`]). The **structural**
/// variants (`AddTask` / `RemoveTask` / `RetargetDeps` / `SplitTask`) map
/// onto the store's revision-fenced `replan` after the resulting graph is
/// validated; **`Retitle`** is spec-only and does not interrupt children or
/// bump the generation (P2-c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanEdit {
    /// Append a brand-new task.
    AddTask(TaskSpec),
    /// Drop a task (its child is `Cancelled` + any reservation released).
    RemoveTask(String),
    /// Rewrite a task's `title` + `detail` (spec-only; no state change).
    Retitle {
        task_id: String,
        title: String,
        detail: String,
    },
    /// Replace a task's dependency set (readiness is recomputed).
    RetargetDeps { task_id: String, deps: Vec<String> },
    /// Replace one task with several: the original is removed (child
    /// `Cancelled`) and the `into` sub-tasks are inserted in its place.
    SplitTask {
        task_id: String,
        into: Vec<TaskSpec>,
    },
    /// PR B — WIDEN a task's operator [`WorkerGrant`] and resume its blocked
    /// child (`Blocked → Ready`), clearing its `pending_escalation`. This is the
    /// keeper's `goal_grant` after a worker escalated mid-task. Like `Retitle`
    /// it is TARGETED — it bumps only the plan `revision` (never `generation`)
    /// and touches only this task's row, NOT a `replan` blast: the yielded
    /// attempt is already settled, so there is no live attempt to interrupt.
    SetGrant { task_id: String, grant: WorkerGrant },
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// The joined task graph a keeper renders: the durable plan (spec) joined
/// with each task's live child (execution state), plus fleet-level status,
/// budget, generation, and plan revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetView {
    pub fleet_id: String,
    pub status: FleetStatus,
    pub budget: FleetBudget,
    pub generation: u64,
    pub revision: u64,
    pub objective: String,
    pub tasks: Vec<TaskView>,
}

/// One row of a [`FleetView`]: the task's spec fields joined with the live
/// state read off its child. `verdict` is the child's terminal
/// [`AcceptanceVerdict`]; `evidence` is projected out of an `Accepted`
/// verdict (evidence's one home is the outcome).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    // ---- spec (from the plan) ----
    pub task_id: String,
    pub title: String,
    pub detail: String,
    pub deps: Vec<String>,
    pub acceptance: Vec<AcceptanceCriterion>,
    /// PR A — the operator grant the worker for this task is built from,
    /// projected from the plan's [`PlanTask::grant`]. The executor
    /// (`octos-fleet-worker`) reads it to build the worker's registry, sandbox
    /// network, and filesystem scope. Defaults to [`WorkerGrant::minimal`].
    pub grant: WorkerGrant,
    // ---- state (from the child) ----
    pub status: ChildStatus,
    pub verdict: Option<AcceptanceVerdict>,
    pub evidence: Vec<EvidenceRef>,
    pub current_attempt_id: Option<String>,
    /// PR B — the pending mid-task escalation, projected from the child's
    /// [`FleetChildRecord::pending_escalation`]. `Some` while `status ==
    /// Blocked`: how the keeper NOTICES a worker's grant-widen request on its
    /// next `goal_get` turn (the request's advisory grant + reason). Cleared
    /// once the keeper's `goal_grant`/`goal_deny` resolves it.
    pub pending_escalation: Option<EscalationRequest>,
}

/// Counts of the current plan's tasks by child state, for synthesis /
/// progress rendering. Counts are over the **current plan's** tasks (a
/// removed task's orphaned `Cancelled` child is not in the plan, so it does
/// not inflate `cancelled`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetSummary {
    pub fleet_id: String,
    pub status: FleetStatus,
    pub generation: u64,
    pub revision: u64,
    pub budget: FleetBudget,
    pub total: usize,
    pub planned: usize,
    pub ready: usize,
    pub launching: usize,
    pub running: usize,
    /// PR B — children yielded on a pending escalation (non-terminal), awaiting
    /// an operator `goal_grant`/`goal_deny`. Counted separately so a synthesis /
    /// progress render distinguishes "waiting on the operator" from in-flight.
    pub blocked: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
}

// ---------------------------------------------------------------------------
// Fleet handle
// ---------------------------------------------------------------------------

/// An ergonomic handle to one fleet's durable plan, over a shared
/// [`FleetKernelStore`]. Cheaply cloneable (`Arc` store + id).
#[derive(Clone)]
pub struct Fleet {
    store: Arc<FleetKernelStore>,
    fleet_id: String,
}

impl Fleet {
    /// Create a fleet + its durable plan + one child per task. The whole
    /// task graph is **validated before any write** (P2-a: unique ids, no
    /// self / dangling / cyclic deps) — a bad graph creates *nothing* — and
    /// the fleet + plan + children are then written in **one** store
    /// transaction ([`FleetKernelStore::create_fleet_with_plan`]), so there
    /// is no half-created fleet on a mid-sequence failure. A task with no
    /// `deps` starts `Ready`; one with `deps` starts `Planned` (promoted
    /// once its deps `Succeed`).
    ///
    /// Only `budget.token_budget` + `budget.hard` are read here — a fresh
    /// fleet always starts with `tokens_reserved == tokens_committed == 0`.
    /// A duplicate `fleet_id` is rejected. The follow-on decision-log append
    /// is advisory (a missing entry is benign).
    #[allow(clippy::too_many_arguments)] // fleet identity + budget + plan inputs are irreducible here
    pub async fn create(
        store: Arc<FleetKernelStore>,
        fleet_id: impl Into<String>,
        controller_session_key: SessionKey,
        controller_workspace_root: Option<String>,
        profile_id: impl Into<String>,
        budget: FleetBudget,
        objective: impl Into<String>,
        tasks: Vec<TaskSpec>,
        now_ms: u64,
    ) -> Result<Fleet> {
        Self::create_with_workspace_provenance(
            store,
            fleet_id,
            controller_session_key,
            controller_workspace_root,
            None,
            profile_id,
            budget,
            objective,
            tasks,
            now_ms,
        )
        .await
    }

    /// Create a fleet while preserving the provenance of its controller
    /// workspace across durable wake/restart boundaries. `None` provenance is
    /// the legacy/unknown case and must never be treated as a cwd hint.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_workspace_provenance(
        store: Arc<FleetKernelStore>,
        fleet_id: impl Into<String>,
        controller_session_key: SessionKey,
        controller_workspace_root: Option<String>,
        controller_workspace_has_runtime_hint: Option<bool>,
        profile_id: impl Into<String>,
        budget: FleetBudget,
        objective: impl Into<String>,
        tasks: Vec<TaskSpec>,
        now_ms: u64,
    ) -> Result<Fleet> {
        let fleet_id = fleet_id.into();
        let profile_id = profile_id.into();
        let tasks: Vec<PlanTask> = tasks.into_iter().map(PlanTask::from).collect();
        let task_count = tasks.len();

        // P2-a: validate the WHOLE graph before any write, so a bad graph
        // (duplicate id, self / dangling / cyclic dep) creates NOTHING.
        validate_graph(&tasks)?;

        let plan = DurablePlan {
            schema_version: SCHEMA_VERSION,
            fleet_id: fleet_id.clone(),
            revision: 0,
            objective: objective.into(),
            tasks,
        };

        // P2-a: fleet + plan + children in ONE transaction (all-or-nothing).
        store
            .create_fleet_with_plan_and_workspace_provenance(
                controller_session_key,
                controller_workspace_root,
                controller_workspace_has_runtime_hint,
                &profile_id,
                budget.token_budget,
                budget.hard,
                plan,
                now_ms,
            )
            .await?;

        // The fleet+plan+children are durable; the decision log is advisory,
        // so append it best-effort (a log failure must NOT turn a committed
        // create into an Err — see `note`).
        let fleet = Fleet { store, fleet_id };
        fleet
            .note(
                DecisionKind::Plan,
                &format!("created fleet with {task_count} task(s)"),
                now_ms,
            )
            .await;
        Ok(fleet)
    }

    /// Attach a handle to an already-created fleet (e.g. after a reboot, or
    /// from an outbox event's `fleet_id`). No I/O; the fleet is read lazily
    /// by the ops.
    pub fn bind(store: Arc<FleetKernelStore>, fleet_id: impl Into<String>) -> Self {
        Fleet {
            store,
            fleet_id: fleet_id.into(),
        }
    }

    /// This fleet's id.
    pub fn fleet_id(&self) -> &str {
        &self.fleet_id
    }

    /// The underlying store — for ops `Fleet` intentionally does not wrap
    /// (attempt launch/run, outbox, recovery), which belong to the executor
    /// and keeper PRs.
    pub fn store(&self) -> &Arc<FleetKernelStore> {
        &self.store
    }

    /// The joined task graph: each task's spec joined with its child's live
    /// state, plus fleet status / budget / generation / revision. Reads an
    /// internally-consistent [`crate::FleetSnapshot`] (P1-c) — plan, fleet,
    /// and children from one instant — so a concurrent `replan` cannot tear
    /// the join into an old-plan + new-children mix.
    pub async fn view(&self) -> Result<FleetView> {
        let loaded = self.load().await?;
        let tasks = loaded
            .plan
            .tasks
            .iter()
            .map(|t| {
                let child = loaded.children.get(&t.task_id);
                let verdict = child.and_then(|c| c.outcome.clone());
                TaskView {
                    task_id: t.task_id.clone(),
                    title: t.title.clone(),
                    detail: t.detail.clone(),
                    deps: t.deps.clone(),
                    acceptance: t.acceptance.clone(),
                    grant: t.grant.clone(),
                    status: child.map(|c| c.status).unwrap_or(ChildStatus::Planned),
                    evidence: verdict_evidence(verdict.as_ref()),
                    verdict,
                    current_attempt_id: child.and_then(|c| c.current_attempt_id.clone()),
                    pending_escalation: child.and_then(|c| c.pending_escalation.clone()),
                }
            })
            .collect();

        Ok(FleetView {
            fleet_id: self.fleet_id.clone(),
            status: loaded.fleet.status,
            budget: loaded.fleet.budget.clone(),
            generation: loaded.fleet.generation,
            revision: loaded.plan.revision,
            objective: loaded.plan.objective.clone(),
            tasks,
        })
    }

    /// The launchable set (sorted by task id): tasks whose child is `Ready`
    /// and has no live attempt.
    ///
    /// **Self-healing, atomically (round-2 P1).** Readiness promotion
    /// (`Planned → Ready`) is normally driven eagerly by
    /// [`Fleet::record_outcome`], but that promotion is a *separate* commit
    /// from the completion, so a crash / wake-ordering between them could
    /// leave a dependent `Planned` even though its final dep already
    /// `Succeeded`. This call delegates to
    /// [`FleetKernelStore::resolve_and_collect_ready`], which promotes every
    /// eligible `Planned` child AND collects the `Ready` set in **one** write
    /// transaction — so the promote-decision and the collect can never tear
    /// across a concurrent completion, and every id returned is genuinely
    /// `Ready` (i.e. `launch_child` will accept it). `now_ms` timestamps any
    /// healed promotion. The `Fleet` facade signature is unchanged.
    pub async fn ready_tasks(&self, now_ms: u64) -> Result<Vec<String>> {
        self.store
            .resolve_and_collect_ready(&self.fleet_id, now_ms)
            .await
    }

    /// Apply a high-level [`PlanEdit`].
    ///
    /// **Structural edits** (`AddTask` / `RemoveTask` / `RetargetDeps` /
    /// `SplitTask`) build a new plan from the *current* one + the edit
    /// (existing task specs preserved verbatim), **validate the resulting
    /// task graph** (P1-a: unique ids, no self / dangling / cyclic deps —
    /// so an edit can never leave a dependent wedged on a removed id), then
    /// map onto the store's revision-fenced `replan` (interrupt live
    /// attempts, bump `generation`, cancel removed tasks + release their
    /// reservations, add new ones, recompute readiness).
    ///
    /// **`Retitle`** is *spec-only* and routes through
    /// [`FleetKernelStore::retitle_task`]: it bumps the plan `revision` but
    /// does **not** bump `generation` or interrupt any child (P2-c).
    ///
    /// Rejections: a stale `expected_revision` → typed
    /// [`PlanMutateOutcome::RevisionMismatch`] (nothing changes). A
    /// structurally-invalid edit → a typed [`PlanGraphError`] (`Err`, no
    /// mutation): unknown / duplicate / cyclic / dangling / self-dep,
    /// `SplitTask` reusing an existing id (P1-a), an empty split, or
    /// retargeting a **terminal** task whose spec is frozen (P2-d).
    pub async fn apply_edit(
        &self,
        edit: PlanEdit,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<PlanMutateOutcome> {
        let snap = self
            .store
            .load_snapshot(&self.fleet_id)
            .await?
            .ok_or_else(|| eyre::eyre!("apply_edit: fleet {} not found", self.fleet_id))?;
        let plan = snap
            .plan
            .ok_or_else(|| eyre::eyre!("apply_edit: no plan for fleet {}", self.fleet_id))?;

        // Fence a stale edit up front (the store op re-fences too).
        if plan.revision != expected_revision {
            return Ok(PlanMutateOutcome::RevisionMismatch {
                actual: plan.revision,
            });
        }

        // Retitle: spec-only, must not interrupt children / bump generation.
        if let PlanEdit::Retitle {
            task_id,
            title,
            detail,
        } = &edit
        {
            if !plan.tasks.iter().any(|t| &t.task_id == task_id) {
                return Err(PlanGraphError::UnknownTask(task_id.clone()).into());
            }
            let outcome = self
                .store
                .retitle_task(
                    &self.fleet_id,
                    expected_revision,
                    task_id,
                    title,
                    detail,
                    now_ms,
                )
                .await?;
            if matches!(outcome, PlanMutateOutcome::Mutated { .. }) {
                self.note(
                    DecisionKind::Note,
                    &format!("retitle task {task_id}"),
                    now_ms,
                )
                .await;
            }
            return Ok(outcome);
        }

        // PR B — SetGrant: targeted grant-widen + Blocked→Ready resume. Like
        // Retitle it must NOT interrupt other children or bump generation, so it
        // routes to the dedicated `set_task_grant` store op, never `replan`.
        if let PlanEdit::SetGrant { task_id, grant } = &edit {
            if !plan.tasks.iter().any(|t| &t.task_id == task_id) {
                return Err(PlanGraphError::UnknownTask(task_id.clone()).into());
            }
            let outcome = self
                .store
                .set_task_grant(
                    &self.fleet_id,
                    expected_revision,
                    task_id,
                    grant.clone(),
                    now_ms,
                )
                .await?;
            if matches!(outcome, PlanMutateOutcome::Mutated { .. }) {
                self.note(
                    DecisionKind::Note,
                    &format!("grant widen + resume task {task_id}"),
                    now_ms,
                )
                .await;
            }
            return Ok(outcome);
        }

        // P2-d: a terminal task's spec is frozen — reject retargeting its
        // deps (it would keep its terminal outcome while dependents run
        // against a changed prerequisite set).
        if let PlanEdit::RetargetDeps { task_id, .. } = &edit {
            if plan.tasks.iter().any(|t| &t.task_id == task_id) {
                if let Some(c) = snap.children.iter().find(|c| &c.child_id == task_id) {
                    if c.status.is_terminal() {
                        return Err(PlanGraphError::RetargetTerminalTask(task_id.clone()).into());
                    }
                }
            }
        }

        // Structural edit → build + validate the graph (P1-a) → replan.
        let mut tasks = plan.tasks.clone();
        let note = apply_edit_to_tasks(&mut tasks, edit)?;
        validate_graph(&tasks)?;

        let new_plan = DurablePlan {
            schema_version: SCHEMA_VERSION,
            fleet_id: self.fleet_id.clone(),
            // `replan` overwrites this to `expected_revision + 1`.
            revision: expected_revision,
            objective: plan.objective.clone(),
            tasks,
        };

        let outcome = self
            .store
            .replan(&self.fleet_id, expected_revision, new_plan, now_ms)
            .await?;

        // The in-txn terminal freeze (round-2 P1) surfaces as the SAME typed
        // error the out-of-txn pre-check uses, so a caller sees one contract
        // whether the task was terminal at the snapshot or completed in the
        // window before `replan`'s CAS.
        if let PlanMutateOutcome::RejectedTerminalDepChange { task_id } = outcome {
            return Err(PlanGraphError::RetargetTerminalTask(task_id).into());
        }
        if matches!(outcome, PlanMutateOutcome::Mutated { .. }) {
            self.note(DecisionKind::Replan, &note, now_ms).await;
        }
        Ok(outcome)
    }

    /// Record an executor result for one attempt: persist the
    /// [`AcceptanceVerdict`] (which carries evidence) via the store's
    /// four-part `complete_child` CAS, append a `Complete`
    /// [`DecisionEntry`], and re-resolve dependents' readiness so a
    /// newly-`Succeeded` task unblocks the tasks that depend on it.
    ///
    /// Returns the typed [`CompleteOutcome`]: `Superseded` (a stale / late
    /// attempt failing the CAS predicate) changes nothing and appends no
    /// decision.
    ///
    /// PR-2 has no executor, so the store's `result_snapshot` is synthesized
    /// from the verdict + `actual_tokens`; PR 3's executor threads the real
    /// `TaskResult` snapshot.
    pub async fn record_outcome(
        &self,
        task_id: &str,
        attempt_id: &str,
        verdict: AcceptanceVerdict,
        actual_tokens: u64,
        owner_epoch: u64,
        now_ms: u64,
    ) -> Result<CompleteOutcome> {
        let snapshot = snapshot_from_verdict(&verdict, actual_tokens);
        let note = describe_verdict(task_id, &verdict);

        let outcome = self
            .store
            .complete_child(
                &self.fleet_id,
                task_id,
                attempt_id,
                verdict,
                snapshot,
                actual_tokens,
                owner_epoch,
                now_ms,
            )
            .await?;

        // The completion is durable. Both follow-ons are best-effort — the
        // decision log is advisory, and the eager readiness promotion is only
        // an optimization (ready_tasks self-heals a miss). Neither may turn a
        // committed completion into an Err (round-2 P2).
        if outcome == CompleteOutcome::Completed {
            self.note(DecisionKind::Complete, &note, now_ms).await;
            self.resolve_ready(now_ms).await;
        }
        Ok(outcome)
    }

    /// True iff every task in the current plan has an `Accepted` child
    /// (`ChildStatus::Succeeded`). A `Failed` / `Cancelled` task is terminal
    /// but *not* accepted, so it holds completion open. An empty plan is
    /// vacuously complete.
    pub async fn is_complete(&self) -> Result<bool> {
        let loaded = self.load().await?;
        Ok(loaded.plan.tasks.iter().all(|t| {
            loaded
                .children
                .get(&t.task_id)
                .map(|c| c.status == ChildStatus::Succeeded)
                .unwrap_or(false)
        }))
    }

    /// Counts of the current plan's tasks by child state (for synthesis).
    pub async fn summary(&self) -> Result<FleetSummary> {
        let loaded = self.load().await?;
        let mut s = FleetSummary {
            fleet_id: self.fleet_id.clone(),
            status: loaded.fleet.status,
            generation: loaded.fleet.generation,
            revision: loaded.plan.revision,
            budget: loaded.fleet.budget.clone(),
            total: loaded.plan.tasks.len(),
            planned: 0,
            ready: 0,
            launching: 0,
            running: 0,
            blocked: 0,
            succeeded: 0,
            failed: 0,
            cancelled: 0,
        };
        for t in &loaded.plan.tasks {
            let status = loaded
                .children
                .get(&t.task_id)
                .map(|c| c.status)
                .unwrap_or(ChildStatus::Planned);
            match status {
                ChildStatus::Planned => s.planned += 1,
                ChildStatus::Ready => s.ready += 1,
                ChildStatus::Launching => s.launching += 1,
                ChildStatus::Running => s.running += 1,
                ChildStatus::Blocked => s.blocked += 1,
                ChildStatus::Succeeded => s.succeeded += 1,
                ChildStatus::Failed => s.failed += 1,
                ChildStatus::Cancelled => s.cancelled += 1,
            }
        }
        Ok(s)
    }

    // ---- internals --------------------------------------------------------

    /// Best-effort advisory decision-log append (round-2 P2). The durable
    /// state machine — not the audit log — is load-bearing, so a log-write
    /// failure must never turn an already-committed state transition into an
    /// `Err`. On failure it warns and continues; it returns nothing.
    async fn note(&self, kind: DecisionKind, note: &str, now_ms: u64) {
        if let Err(e) = self
            .store
            .append_decision(&self.fleet_id, "keeper", kind, note, now_ms)
            .await
        {
            eprintln!(
                "octos-fleet: decision-log append failed for fleet {} (non-fatal): {e}",
                self.fleet_id
            );
        }
    }

    /// Eagerly re-resolve readiness after a completion: promote every
    /// `Planned` child whose deps are now all `Succeeded` (via the store's
    /// `mark_ready`, a no-op otherwise). Best-effort — a missed promotion is
    /// self-healed by [`Fleet::ready_tasks`] (round-2 P1), so this is only an
    /// optimization to show `Ready` promptly in `view`/`summary`; a failure
    /// must not fail the committed completion (round-2 P2).
    async fn resolve_ready(&self, now_ms: u64) {
        let children = match self.store.list_children(&self.fleet_id).await {
            Ok(children) => children,
            Err(e) => {
                eprintln!(
                    "octos-fleet: eager readiness resolve skipped for fleet {} (non-fatal): {e}",
                    self.fleet_id
                );
                return;
            }
        };
        for c in &children {
            if c.status == ChildStatus::Planned {
                if let Err(e) = self
                    .store
                    .mark_ready(&self.fleet_id, &c.child_id, now_ms)
                    .await
                {
                    eprintln!(
                        "octos-fleet: eager mark_ready({}) failed (non-fatal): {e}",
                        c.child_id
                    );
                }
            }
        }
    }

    /// Load an internally-consistent snapshot (P1-c) — fleet + plan +
    /// children from ONE read transaction — keyed by child_id for the joins.
    async fn load(&self) -> Result<Loaded> {
        let snap = self
            .store
            .load_snapshot(&self.fleet_id)
            .await?
            .ok_or_else(|| eyre::eyre!("fleet {} not found", self.fleet_id))?;
        let plan = snap
            .plan
            .ok_or_else(|| eyre::eyre!("plan for fleet {} not found", self.fleet_id))?;
        let children = snap
            .children
            .into_iter()
            .map(|c| (c.child_id.clone(), c))
            .collect();
        Ok(Loaded {
            fleet: snap.fleet,
            plan,
            children,
        })
    }
}

/// One joined read: fleet + plan + children-by-id.
struct Loaded {
    fleet: crate::records::FleetRecord,
    plan: DurablePlan,
    children: HashMap<String, FleetChildRecord>,
}

// ---------------------------------------------------------------------------
// Edit application + graph validation (pure, over the task list)
// ---------------------------------------------------------------------------

/// A typed rejection of a structurally-invalid plan edit / graph (P1-a,
/// P2-a, P2-d). Distinct from the store's *concurrency* rejections (which
/// are `PlanMutateOutcome::RevisionMismatch` / `CompleteOutcome::Superseded`
/// **values**): these are caller mistakes surfaced as an `Err`, and remain
/// downcastable from the returned `eyre::Report` via
/// `downcast_ref::<PlanGraphError>()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanGraphError {
    /// Two tasks share a `task_id`.
    DuplicateTask(String),
    /// `AddTask` for an id already in the plan.
    DuplicateAdd(String),
    /// An edit targets a task that is not in the plan.
    UnknownTask(String),
    /// A task lists itself as a dependency.
    SelfDependency(String),
    /// A dependency references a task that is not in the plan.
    DanglingDependency { task: String, dep: String },
    /// The dependency graph contains a cycle (the involved task_ids).
    Cycle(Vec<String>),
    /// `SplitTask` with an empty `into` list.
    EmptySplit(String),
    /// A `SplitTask` sub-task reuses the split id (or another existing id);
    /// sub-task ids must be brand new, else `replan` would treat an
    /// already-terminal child as a survivor of the new spec (false
    /// completion).
    SplitIdReuse(String),
    /// `RetargetDeps` on a terminal (frozen-spec) task.
    RetargetTerminalTask(String),
}

impl std::fmt::Display for PlanGraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanGraphError::DuplicateTask(id) => write!(f, "duplicate task id {id:?} in the plan"),
            PlanGraphError::DuplicateAdd(id) => write!(f, "AddTask: task {id:?} already exists"),
            PlanGraphError::UnknownTask(id) => write!(f, "edit targets unknown task {id:?}"),
            PlanGraphError::SelfDependency(id) => write!(f, "task {id:?} depends on itself"),
            PlanGraphError::DanglingDependency { task, dep } => {
                write!(f, "task {task:?} depends on nonexistent task {dep:?}")
            }
            PlanGraphError::Cycle(ids) => write!(f, "dependency cycle among {ids:?}"),
            PlanGraphError::EmptySplit(id) => {
                write!(f, "SplitTask: {id:?} must split into at least one sub-task")
            }
            PlanGraphError::SplitIdReuse(id) => write!(
                f,
                "SplitTask: sub-task id {id:?} must be new (it reuses an existing id)"
            ),
            PlanGraphError::RetargetTerminalTask(id) => {
                write!(
                    f,
                    "RetargetDeps: task {id:?} is terminal; its spec is frozen"
                )
            }
        }
    }
}

impl std::error::Error for PlanGraphError {}

/// Apply `edit` to `tasks` in place, returning a decision-log note. Only
/// structural checks that need the edit's own shape live here (dup add,
/// unknown target, empty split, `SplitTask` id reuse); the whole-graph
/// invariants are then re-checked by [`validate_graph`].
fn apply_edit_to_tasks(
    tasks: &mut Vec<PlanTask>,
    edit: PlanEdit,
) -> std::result::Result<String, PlanGraphError> {
    match edit {
        PlanEdit::AddTask(spec) => {
            if tasks.iter().any(|t| t.task_id == spec.task_id) {
                return Err(PlanGraphError::DuplicateAdd(spec.task_id));
            }
            let id = spec.task_id.clone();
            tasks.push(spec.into());
            Ok(format!("add task {id}"))
        }
        PlanEdit::RemoveTask(id) => {
            let before = tasks.len();
            tasks.retain(|t| t.task_id != id);
            if tasks.len() == before {
                return Err(PlanGraphError::UnknownTask(id));
            }
            Ok(format!("remove task {id}"))
        }
        PlanEdit::Retitle {
            task_id,
            title,
            detail,
        } => {
            let t = tasks
                .iter_mut()
                .find(|t| t.task_id == task_id)
                .ok_or_else(|| PlanGraphError::UnknownTask(task_id.clone()))?;
            t.title = title;
            t.detail = detail;
            Ok(format!("retitle task {task_id}"))
        }
        PlanEdit::RetargetDeps { task_id, deps } => {
            let t = tasks
                .iter_mut()
                .find(|t| t.task_id == task_id)
                .ok_or_else(|| PlanGraphError::UnknownTask(task_id.clone()))?;
            t.deps = deps;
            Ok(format!("retarget deps of task {task_id}"))
        }
        // PR B: SetGrant is spec-only (like Retitle) and is early-returned by
        // `apply_edit` to `set_task_grant`, so it never reaches this structural
        // path. The arm exists for exhaustiveness and does the pure spec change.
        PlanEdit::SetGrant { task_id, grant } => {
            let t = tasks
                .iter_mut()
                .find(|t| t.task_id == task_id)
                .ok_or_else(|| PlanGraphError::UnknownTask(task_id.clone()))?;
            t.grant = grant;
            Ok(format!("set grant of task {task_id}"))
        }
        PlanEdit::SplitTask { task_id, into } => {
            if into.is_empty() {
                return Err(PlanGraphError::EmptySplit(task_id));
            }
            let pos = tasks
                .iter()
                .position(|t| t.task_id == task_id)
                .ok_or_else(|| PlanGraphError::UnknownTask(task_id.clone()))?;
            // P1-a: sub-task ids MUST be brand new. Reusing the split id (or
            // any other existing id) would make `replan` treat an
            // already-terminal child as a survivor of the new spec.
            let existing: std::collections::HashSet<String> =
                tasks.iter().map(|t| t.task_id.clone()).collect();
            for sub in &into {
                if existing.contains(&sub.task_id) {
                    return Err(PlanGraphError::SplitIdReuse(sub.task_id.clone()));
                }
            }
            tasks.remove(pos);
            for (i, spec) in into.into_iter().enumerate() {
                tasks.insert(pos + i, spec.into());
            }
            Ok(format!("split task {task_id}"))
        }
    }
}

/// Validate the whole task graph (P1-a): unique ids, no self-dependency, no
/// dangling dependency (a dep that references a task not in the plan), and no
/// cycle. Rejecting these is what stops an edit from leaving a durable wedge
/// (a child `Planned` forever because a dep can never `Succeed`).
fn validate_graph(tasks: &[PlanTask]) -> std::result::Result<(), PlanGraphError> {
    let mut ids = std::collections::HashSet::new();
    for t in tasks {
        if !ids.insert(t.task_id.as_str()) {
            return Err(PlanGraphError::DuplicateTask(t.task_id.clone()));
        }
    }
    for t in tasks {
        for d in &t.deps {
            if d == &t.task_id {
                return Err(PlanGraphError::SelfDependency(t.task_id.clone()));
            }
            if !ids.contains(d.as_str()) {
                return Err(PlanGraphError::DanglingDependency {
                    task: t.task_id.clone(),
                    dep: d.clone(),
                });
            }
        }
    }
    if let Some(cycle) = find_cycle(tasks) {
        return Err(PlanGraphError::Cycle(cycle));
    }
    Ok(())
}

/// Cycle detection via Kahn's algorithm (iterative topological sort). Deps
/// are assumed already dangling-checked. Returns the ids left unresolved
/// (i.e. participating in / downstream of a cycle), sorted, or `None`.
fn find_cycle(tasks: &[PlanTask]) -> Option<Vec<String>> {
    let ids: std::collections::HashSet<&str> = tasks.iter().map(|t| t.task_id.as_str()).collect();
    let mut indeg: std::collections::HashMap<&str, usize> =
        tasks.iter().map(|t| (t.task_id.as_str(), 0usize)).collect();
    let mut enables: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for t in tasks {
        for d in &t.deps {
            if ids.contains(d.as_str()) {
                if let Some(v) = indeg.get_mut(t.task_id.as_str()) {
                    *v += 1;
                }
                enables
                    .entry(d.as_str())
                    .or_default()
                    .push(t.task_id.as_str());
            }
        }
    }
    let mut queue: std::collections::VecDeque<&str> = indeg
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut processed = 0usize;
    while let Some(n) = queue.pop_front() {
        processed += 1;
        if let Some(ts) = enables.get(n) {
            for &t in ts {
                if let Some(v) = indeg.get_mut(t) {
                    *v -= 1;
                    if *v == 0 {
                        queue.push_back(t);
                    }
                }
            }
        }
    }
    if processed == tasks.len() {
        None
    } else {
        let mut cyc: Vec<String> = indeg
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(id, _)| id.to_string())
            .collect();
        cyc.sort();
        Some(cyc)
    }
}

// ---------------------------------------------------------------------------
// Verdict helpers
// ---------------------------------------------------------------------------

/// Evidence is projected out of an `Accepted` verdict — its one home is the
/// child's outcome, so the view derives it rather than storing a copy.
fn verdict_evidence(verdict: Option<&AcceptanceVerdict>) -> Vec<EvidenceRef> {
    match verdict {
        Some(AcceptanceVerdict::Accepted { evidence }) => evidence.clone(),
        _ => Vec::new(),
    }
}

/// Synthesize the store's `result_snapshot` from a verdict (PR-2 stand-in;
/// PR 3's executor supplies the real `TaskResult` snapshot).
fn snapshot_from_verdict(verdict: &AcceptanceVerdict, tokens: u64) -> ChildResultSnapshot {
    match verdict {
        AcceptanceVerdict::Accepted { .. } => ChildResultSnapshot {
            output: "accepted".into(),
            success: true,
            tokens_used: tokens,
            files: Vec::new(),
            error: None,
        },
        AcceptanceVerdict::Rejected { reason } | AcceptanceVerdict::Terminated { reason } => {
            ChildResultSnapshot {
                output: String::new(),
                success: false,
                tokens_used: tokens,
                files: Vec::new(),
                error: Some(reason.clone()),
            }
        }
    }
}

fn describe_verdict(task_id: &str, verdict: &AcceptanceVerdict) -> String {
    match verdict {
        AcceptanceVerdict::Accepted { evidence } => {
            format!("{task_id}: accepted ({} evidence)", evidence.len())
        }
        AcceptanceVerdict::Rejected { reason } => format!("{task_id}: rejected — {reason}"),
        AcceptanceVerdict::Terminated { reason } => format!("{task_id}: terminated — {reason}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::{AttemptStatus, ChildResultSnapshot, Verifier};
    use crate::store::LaunchOutcome;
    use tempfile::TempDir;

    const EPOCH: u64 = 7;
    const TTL: u64 = 1_000;

    async fn fresh() -> (TempDir, Arc<FleetKernelStore>) {
        let dir = TempDir::new().unwrap();
        let store = FleetKernelStore::open(dir.path()).await.unwrap();
        (dir, Arc::new(store))
    }

    fn controller() -> SessionKey {
        SessionKey::new("fleet", "keeper-1")
    }

    fn budget(cap: u64) -> FleetBudget {
        FleetBudget {
            token_budget: cap,
            tokens_reserved: 0,
            tokens_committed: 0,
            hard: false,
        }
    }

    fn spec(id: &str, deps: &[&str]) -> TaskSpec {
        TaskSpec {
            task_id: id.into(),
            title: format!("Task {id}"),
            detail: "do the thing".into(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            acceptance: Vec::new(),
            grant: WorkerGrant::minimal(),
        }
    }

    fn accepted() -> AcceptanceVerdict {
        AcceptanceVerdict::Accepted {
            evidence: vec![EvidenceRef {
                kind: "file".into(),
                locator: "out.txt".into(),
                sha256: "abc123".into(),
                captured_at_ms: 1,
            }],
        }
    }

    fn rejected() -> AcceptanceVerdict {
        AcceptanceVerdict::Rejected {
            reason: "did not pass acceptance".into(),
        }
    }

    /// Drive a `Ready` task through the store CAS to a recorded outcome:
    /// launch → mark_running → `Fleet::record_outcome`.
    async fn run_and_record(
        fleet: &Fleet,
        task: &str,
        verdict: AcceptanceVerdict,
        now: u64,
    ) -> CompleteOutcome {
        let store = fleet.store();
        let attempt = match store
            .launch_child(fleet.fleet_id(), task, 100, now, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("launch {task}: {other:?}"),
        };
        store.mark_running(task, &attempt).await.unwrap();
        fleet
            .record_outcome(task, &attempt, verdict, 80, EPOCH, now)
            .await
            .unwrap()
    }

    /// Drive a `Ready` task into `Blocked` via a recorded escalation: launch →
    /// mark_running → `record_escalation` (80 real tokens). Returns the request
    /// that was recorded, for assertions.
    async fn escalate_child(fleet: &Fleet, task: &str, now: u64) -> crate::EscalationRequest {
        let store = fleet.store();
        let attempt = match store
            .launch_child(fleet.fleet_id(), task, 100, now, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("launch {task}: {other:?}"),
        };
        store.mark_running(task, &attempt).await.unwrap();
        let request = crate::EscalationRequest {
            requested_grant: WorkerGrant {
                network: crate::grant::NetworkGrant::Hosts(vec!["example.com".into()]),
                tools: vec!["read_file".into(), "web_fetch".into()],
                ..WorkerGrant::minimal()
            },
            reason: format!("{task} needs example.com"),
        };
        let out = store
            .record_escalation(
                fleet.fleet_id(),
                task,
                &attempt,
                request.clone(),
                80,
                EPOCH,
                now,
            )
            .await
            .unwrap();
        assert_eq!(out, CompleteOutcome::Completed);
        request
    }

    #[tokio::test]
    async fn set_grant_edit_widens_task_grant_and_readies_it() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[]), spec("b", &[])],
            0,
        )
        .await
        .unwrap();
        // Drive `a` to Blocked; `b` stays a fresh Ready, minimal-grant control.
        escalate_child(&fleet, "a", 1).await;
        let before = fleet.view().await.unwrap();
        assert_eq!(before.revision, 0);
        assert_eq!(before.generation, 0);
        let a_before = before.tasks.iter().find(|t| t.task_id == "a").unwrap();
        assert_eq!(a_before.status, ChildStatus::Blocked);
        assert!(
            a_before.pending_escalation.is_some(),
            "a is blocked on a request"
        );

        // The keeper grants a WIDER grant (web_fetch under a Hosts allowlist).
        let wider = WorkerGrant {
            network: crate::grant::NetworkGrant::Hosts(vec!["example.com".into()]),
            tools: vec!["read_file".into(), "write_file".into(), "web_fetch".into()],
            ..WorkerGrant::minimal()
        };
        let out = fleet
            .apply_edit(
                PlanEdit::SetGrant {
                    task_id: "a".into(),
                    grant: wider.clone(),
                },
                0,
                7,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            PlanMutateOutcome::Mutated { revision: 1 },
            "SetGrant bumps only the revision",
        );

        let after = fleet.view().await.unwrap();
        assert_eq!(after.revision, 1, "revision bumped");
        assert_eq!(
            after.generation, 0,
            "SetGrant must NOT bump the generation (no replan blast)",
        );
        let a = after.tasks.iter().find(|t| t.task_id == "a").unwrap();
        assert_eq!(a.status, ChildStatus::Ready, "Blocked → Ready");
        assert_eq!(a.grant, wider, "the widened grant is applied");
        assert!(
            a.pending_escalation.is_none(),
            "the escalation is cleared on apply",
        );
        // `b` is untouched (no blast radius): still Ready, still minimal.
        let b = after.tasks.iter().find(|t| t.task_id == "b").unwrap();
        assert_eq!(b.status, ChildStatus::Ready);
        assert_eq!(b.grant, WorkerGrant::minimal());
        // The resumed task is launchable again.
        assert!(
            fleet
                .ready_tasks(9)
                .await
                .unwrap()
                .contains(&"a".to_string())
        );
    }

    #[tokio::test]
    async fn blocked_child_keeps_goal_active() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        escalate_child(&fleet, "a", 1).await;
        // A Blocked (non-terminal, unaccepted) child holds completion open: the
        // goal stays ACTIVE while it waits on the operator.
        assert!(
            !fleet.is_complete().await.unwrap(),
            "a Blocked child must keep the fleet incomplete (goal stays active)",
        );
        let s = fleet.summary().await.unwrap();
        assert_eq!(s.blocked, 1, "the summary counts the blocked child");
        assert_eq!(s.succeeded, 0);
    }

    #[tokio::test]
    async fn deny_moves_blocked_to_failed_terminal() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        escalate_child(&fleet, "a", 1).await;

        // Deny: the child must go Blocked → Failed (TERMINAL), else it wedges
        // the fleet forever (is_complete never trips on a non-terminal child).
        let out = store
            .deny_escalation("f1", "a", "no budget for that host", 9)
            .await
            .unwrap();
        assert_eq!(out.settled, CompleteOutcome::Completed);
        // codex round-4 (defect 2): the deny txn ITSELF reports the fleet is now
        // un-completable (a Failed task strands it) so the keeper resolves the
        // goal without a separate read.
        assert!(
            out.fleet_un_completable,
            "denying the only task makes the fleet un-completable",
        );

        let v = fleet.view().await.unwrap();
        let a = v.tasks.iter().find(|t| t.task_id == "a").unwrap();
        assert_eq!(a.status, ChildStatus::Failed, "denial is terminal");
        assert!(a.status.is_terminal(), "a denied child is terminal");
        assert!(matches!(
            a.verdict,
            Some(AcceptanceVerdict::Rejected { .. })
        ));
        assert!(a.pending_escalation.is_none(), "escalation cleared on deny");
        // A denied child is not launchable — the fleet doesn't wedge on it.
        assert!(fleet.ready_tasks(9).await.unwrap().is_empty());

        // A second deny is an inert no-op (the child already resumed/failed) — and
        // a no-op reports no completability change.
        let again = store.deny_escalation("f1", "a", "again", 10).await.unwrap();
        assert_eq!(again.settled, CompleteOutcome::Superseded);
        assert!(
            !again.fleet_un_completable,
            "a no-op deny reports fleet_un_completable = false (nothing changed)",
        );
    }

    #[tokio::test]
    async fn deny_reports_un_completable_over_a_multi_task_plan() {
        // codex round-4 (defect 2): the completability the deny txn RETURNS is
        // computed by SCANNING the plan's children — not just the denied one. A
        // two-task fleet (`b` depends on `a`): denying the escalated `a` makes the
        // fleet un-completable (a Failed task PLUS a still-Planned task), and the
        // scan reads BOTH children — `a` from the txn-local record it just wrote,
        // `b` from the children table.
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"])],
            0,
        )
        .await
        .unwrap();
        escalate_child(&fleet, "a", 1).await;

        let out = store.deny_escalation("f1", "a", "no", 9).await.unwrap();
        assert_eq!(out.settled, CompleteOutcome::Completed);
        assert!(
            out.fleet_un_completable,
            "denying `a` strands dependent `b` — the scanned plan is un-completable",
        );
        // `b` never became Ready (its prerequisite failed), so the fleet is wedged
        // absent a replan — exactly what the returned flag tells the keeper.
        assert!(fleet.ready_tasks(9).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deny_emits_a_childdone_wake_and_bumps_revision() {
        // codex round-2 (defect 3+4): deny must EMIT a ChildDone wake (so the
        // keeper re-evaluates instead of the goal wedging active) AND bump the
        // plan revision (so a concurrent grant@N fails its CAS).
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        escalate_child(&fleet, "a", 1).await;
        let rev_before = fleet.view().await.unwrap().revision;

        store.deny_escalation("f1", "a", "no", 9).await.unwrap();

        // The plan revision advanced (mutual exclusion with a racing grant).
        assert_eq!(
            fleet.view().await.unwrap().revision,
            rev_before + 1,
            "deny bumps the plan revision",
        );
        // At least two ChildDone events exist (one from the escalation, one from
        // the deny) — the deny wake is what re-drives the keeper.
        let mut child_done = 0;
        for _ in 0..8 {
            match store.claim_next("k", 0, 100).await.unwrap() {
                Some(ev) if ev.kind == crate::records::FleetEventKind::ChildDone => child_done += 1,
                Some(_) => {}
                None => break,
            }
        }
        assert!(
            child_done >= 2,
            "deny must emit a ChildDone wake (saw {child_done} total)",
        );
    }

    #[tokio::test]
    async fn grant_after_deny_is_rejected_not_applied() {
        // codex round-2 (defect 4): grant and deny are MUTUALLY EXCLUSIVE. Once
        // deny fails the child, a grant (even one that read `Blocked`) must be
        // REFUSED — the denied capability can never reach the plan.
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        escalate_child(&fleet, "a", 1).await;
        // Deny wins: Blocked → Failed + revision bump (0 → 1).
        store.deny_escalation("f1", "a", "no", 2).await.unwrap();

        let wider = WorkerGrant {
            network: crate::grant::NetworkGrant::Hosts(vec!["example.com".into()]),
            tools: vec!["read_file".into(), "web_fetch".into()],
            ..WorkerGrant::minimal()
        };
        // A grant at the NEW revision (child now Failed, not Blocked) → the in-txn
        // Blocked CAS refuses it (RejectedNotBlocked), no mutation.
        let rev = fleet.view().await.unwrap().revision;
        let refused = fleet
            .apply_edit(
                PlanEdit::SetGrant {
                    task_id: "a".into(),
                    grant: wider.clone(),
                },
                rev,
                3,
            )
            .await
            .unwrap();
        assert!(
            matches!(refused, PlanMutateOutcome::RejectedNotBlocked { .. }),
            "grant on a non-Blocked (denied) child must be refused: {refused:?}",
        );
        // A grant at the STALE (pre-deny) revision → RevisionMismatch (belt).
        let stale = fleet
            .apply_edit(
                PlanEdit::SetGrant {
                    task_id: "a".into(),
                    grant: wider.clone(),
                },
                0,
                4,
            )
            .await
            .unwrap();
        assert!(matches!(stale, PlanMutateOutcome::RevisionMismatch { .. }));
        // The denied capability was NEVER applied — the task keeps its minimal
        // grant and stays terminally Failed.
        let v = fleet.view().await.unwrap();
        let a = v.tasks.iter().find(|t| t.task_id == "a").unwrap();
        assert_eq!(a.status, ChildStatus::Failed);
        assert_eq!(
            a.grant,
            WorkerGrant::minimal(),
            "the denied grant must never be applied",
        );
    }

    #[tokio::test]
    async fn replan_clears_a_blocked_survivors_pending_escalation() {
        // codex round-2 (defect 4): a replan that re-readies a Blocked survivor
        // MUST clear its stale `pending_escalation` — the advisory grant may name
        // capabilities the operator never approved.
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        escalate_child(&fleet, "a", 1).await;
        assert!(
            fleet
                .view()
                .await
                .unwrap()
                .tasks
                .iter()
                .find(|t| t.task_id == "a")
                .unwrap()
                .pending_escalation
                .is_some(),
        );

        // A structural replan (AddTask) resets the Blocked survivor.
        fleet
            .apply_edit(PlanEdit::AddTask(spec("b", &[])), 0, 5)
            .await
            .unwrap();
        let v = fleet.view().await.unwrap();
        let a = v.tasks.iter().find(|t| t.task_id == "a").unwrap();
        assert!(
            a.pending_escalation.is_none(),
            "replan must clear the stale escalation on a re-readied Blocked child",
        );
        assert_eq!(
            a.status,
            ChildStatus::Ready,
            "the Blocked survivor is re-readied by the replan",
        );
    }

    #[tokio::test]
    async fn create_persists_controller_workspace_root() {
        // PR 4b: the controller's workspace root, known at fleet-create time,
        // is persisted on the `FleetRecord` so a headless keeper can be
        // rehydrated across a serve restart. `Some(root)` round-trips; `None`
        // stays `None` (a keeper simply not headlessly rehydratable).
        let (_d, store) = fresh().await;
        Fleet::create(
            store.clone(),
            "f-with-root",
            controller(),
            Some("/repos/app".to_owned()),
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        let rec = store
            .get_fleet("f-with-root")
            .await
            .unwrap()
            .expect("fleet exists");
        assert_eq!(
            rec.controller_workspace_root,
            Some("/repos/app".to_owned()),
            "the create-time workspace root is persisted"
        );

        Fleet::create(
            store.clone(),
            "f-no-root",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        let rec = store
            .get_fleet("f-no-root")
            .await
            .unwrap()
            .expect("fleet exists");
        assert_eq!(
            rec.controller_workspace_root, None,
            "no root persists as None"
        );
    }

    #[tokio::test]
    async fn create_sets_initial_ready_and_planned_from_deps() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"])],
            0,
        )
        .await
        .unwrap();

        let v = fleet.view().await.unwrap();
        assert_eq!(v.revision, 0);
        assert_eq!(v.generation, 0);
        assert_eq!(v.status, FleetStatus::Active);
        assert_eq!(v.budget.token_budget, 1_000);
        let a = v.tasks.iter().find(|t| t.task_id == "a").unwrap();
        let b = v.tasks.iter().find(|t| t.task_id == "b").unwrap();
        assert_eq!(a.status, ChildStatus::Ready, "dep-free task starts Ready");
        assert_eq!(
            b.status,
            ChildStatus::Planned,
            "dep-gated task starts Planned"
        );
        assert_eq!(b.deps, vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn ready_tasks_excludes_a_task_with_an_unmet_dep() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"])],
            0,
        )
        .await
        .unwrap();
        assert_eq!(fleet.ready_tasks(9).await.unwrap(), vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn ready_tasks_excludes_a_launched_child() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        // Launch a via the store: it leaves `Ready`, becomes `Launching`
        // with a live attempt, so it is no longer launchable.
        match store
            .launch_child("f1", "a", 100, 1, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { .. } => {}
            other => panic!("{other:?}"),
        }
        assert!(
            fleet.ready_tasks(9).await.unwrap().is_empty(),
            "an in-flight task is not in the launchable set"
        );
    }

    #[tokio::test]
    async fn record_outcome_accepted_unblocks_dependent() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"])],
            0,
        )
        .await
        .unwrap();
        assert_eq!(fleet.ready_tasks(9).await.unwrap(), vec!["a".to_string()]);

        let out = run_and_record(&fleet, "a", accepted(), 1).await;
        assert_eq!(out, CompleteOutcome::Completed);

        let v = fleet.view().await.unwrap();
        assert_eq!(
            v.tasks.iter().find(|t| t.task_id == "a").unwrap().status,
            ChildStatus::Succeeded
        );
        assert_eq!(
            v.tasks.iter().find(|t| t.task_id == "b").unwrap().status,
            ChildStatus::Ready,
            "the dependent is promoted once its dep Succeeds"
        );
        assert_eq!(fleet.ready_tasks(9).await.unwrap(), vec!["b".to_string()]);
        assert!(!fleet.is_complete().await.unwrap());
    }

    #[tokio::test]
    async fn record_outcome_rejected_marks_failed_and_blocks_dependent() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"])],
            0,
        )
        .await
        .unwrap();

        run_and_record(&fleet, "a", rejected(), 1).await;
        let v = fleet.view().await.unwrap();
        let a = v.tasks.iter().find(|t| t.task_id == "a").unwrap();
        assert_eq!(a.status, ChildStatus::Failed);
        assert!(matches!(
            a.verdict,
            Some(AcceptanceVerdict::Rejected { .. })
        ));
        assert_eq!(
            v.tasks.iter().find(|t| t.task_id == "b").unwrap().status,
            ChildStatus::Planned,
            "a failed dep leaves the dependent blocked"
        );
        assert!(fleet.ready_tasks(9).await.unwrap().is_empty());
        assert!(!fleet.is_complete().await.unwrap());
    }

    #[tokio::test]
    async fn record_outcome_superseded_for_wrong_attempt_is_typed_and_inert() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        let attempt = match store
            .launch_child("f1", "a", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        store.mark_running("a", &attempt).await.unwrap();

        let out = fleet
            .record_outcome("a", "bogus-attempt", accepted(), 80, EPOCH, 1)
            .await
            .unwrap();
        assert_eq!(out, CompleteOutcome::Superseded);
        // Nothing changed; no Complete decision was logged.
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Running
        );
        assert!(
            store
                .list_decisions("f1")
                .await
                .unwrap()
                .iter()
                .all(|d| d.kind != DecisionKind::Complete)
        );
    }

    #[tokio::test]
    async fn is_complete_only_when_all_accepted() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"])],
            0,
        )
        .await
        .unwrap();
        assert!(!fleet.is_complete().await.unwrap());
        run_and_record(&fleet, "a", accepted(), 1).await;
        assert!(!fleet.is_complete().await.unwrap(), "b still pending");
        run_and_record(&fleet, "b", accepted(), 2).await;
        assert!(fleet.is_complete().await.unwrap(), "all tasks Accepted");
    }

    #[tokio::test]
    async fn apply_edit_add_task_bumps_generation_and_adds_ready_child() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();

        let out = fleet
            .apply_edit(PlanEdit::AddTask(spec("c", &[])), 0, 5)
            .await
            .unwrap();
        assert_eq!(out, PlanMutateOutcome::Mutated { revision: 1 });

        let v = fleet.view().await.unwrap();
        assert_eq!(v.revision, 1);
        assert_eq!(v.generation, 1, "replan bumps the generation");
        let c = v.tasks.iter().find(|t| t.task_id == "c").unwrap();
        assert_eq!(c.status, ChildStatus::Ready);
        assert!(
            fleet
                .ready_tasks(9)
                .await
                .unwrap()
                .contains(&"c".to_string())
        );
    }

    #[tokio::test]
    async fn apply_edit_remove_task_cancels_child_and_frees_reservation() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(100),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        // Launch a → reserves the whole budget.
        match store
            .launch_child("f1", "a", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { .. } => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(
            store
                .get_fleet("f1")
                .await
                .unwrap()
                .unwrap()
                .budget
                .tokens_reserved,
            100
        );

        let out = fleet
            .apply_edit(PlanEdit::RemoveTask("a".into()), 0, 5)
            .await
            .unwrap();
        assert_eq!(out, PlanMutateOutcome::Mutated { revision: 1 });
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Cancelled
        );
        assert_eq!(
            store
                .get_fleet("f1")
                .await
                .unwrap()
                .unwrap()
                .budget
                .tokens_reserved,
            0,
            "removing a live child releases its reservation"
        );
        assert!(
            fleet
                .view()
                .await
                .unwrap()
                .tasks
                .iter()
                .all(|t| t.task_id != "a"),
            "the removed task is gone from the plan view"
        );
    }

    #[tokio::test]
    async fn apply_edit_split_task_replaces_one_with_many() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("big", &[])],
            0,
        )
        .await
        .unwrap();

        let out = fleet
            .apply_edit(
                PlanEdit::SplitTask {
                    task_id: "big".into(),
                    into: vec![spec("p1", &[]), spec("p2", &["p1"])],
                },
                0,
                5,
            )
            .await
            .unwrap();
        assert_eq!(out, PlanMutateOutcome::Mutated { revision: 1 });

        let v = fleet.view().await.unwrap();
        assert!(
            v.tasks.iter().all(|t| t.task_id != "big"),
            "big left the plan"
        );
        assert_eq!(
            store.get_child("f1", "big").await.unwrap().unwrap().status,
            ChildStatus::Cancelled
        );
        assert_eq!(
            v.tasks.iter().find(|t| t.task_id == "p1").unwrap().status,
            ChildStatus::Ready
        );
        assert_eq!(
            v.tasks.iter().find(|t| t.task_id == "p2").unwrap().status,
            ChildStatus::Planned
        );
    }

    #[tokio::test]
    async fn apply_edit_retitle_and_retarget_deps() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[]), spec("b", &[])],
            0,
        )
        .await
        .unwrap();
        assert_eq!(
            fleet.ready_tasks(9).await.unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );

        fleet
            .apply_edit(
                PlanEdit::Retitle {
                    task_id: "a".into(),
                    title: "renamed".into(),
                    detail: "new detail".into(),
                },
                0,
                1,
            )
            .await
            .unwrap();
        let v = fleet.view().await.unwrap();
        let a = v.tasks.iter().find(|t| t.task_id == "a").unwrap();
        assert_eq!(a.title, "renamed");
        assert_eq!(a.detail, "new detail");
        assert_eq!(
            a.status,
            ChildStatus::Ready,
            "a retitle does not touch state"
        );

        // Retarget b to depend on the not-yet-Succeeded a → demoted to Planned.
        fleet
            .apply_edit(
                PlanEdit::RetargetDeps {
                    task_id: "b".into(),
                    deps: vec!["a".into()],
                },
                1,
                2,
            )
            .await
            .unwrap();
        let v = fleet.view().await.unwrap();
        let b = v.tasks.iter().find(|t| t.task_id == "b").unwrap();
        assert_eq!(b.status, ChildStatus::Planned, "new unmet dep demotes b");
        assert_eq!(b.deps, vec!["a".to_string()]);
        assert_eq!(fleet.ready_tasks(9).await.unwrap(), vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn apply_edit_stale_revision_is_rejected() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        // Advance to revision 1.
        assert_eq!(
            fleet
                .apply_edit(PlanEdit::AddTask(spec("b", &[])), 0, 1)
                .await
                .unwrap(),
            PlanMutateOutcome::Mutated { revision: 1 }
        );
        // A stale edit at revision 0 is fenced; nothing changes.
        let stale = fleet
            .apply_edit(PlanEdit::AddTask(spec("c", &[])), 0, 2)
            .await
            .unwrap();
        assert_eq!(stale, PlanMutateOutcome::RevisionMismatch { actual: 1 });
        let v = fleet.view().await.unwrap();
        assert_eq!(v.revision, 1);
        assert!(
            v.tasks.iter().all(|t| t.task_id != "c"),
            "stale edit left the plan untouched"
        );
    }

    #[tokio::test]
    async fn apply_edit_rejects_malformed_edits_without_mutating() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();

        assert!(
            fleet
                .apply_edit(PlanEdit::AddTask(spec("a", &[])), 0, 1)
                .await
                .is_err(),
            "duplicate add"
        );
        assert!(
            fleet
                .apply_edit(PlanEdit::RemoveTask("ghost".into()), 0, 1)
                .await
                .is_err(),
            "remove missing"
        );
        assert!(
            fleet
                .apply_edit(
                    PlanEdit::SplitTask {
                        task_id: "ghost".into(),
                        into: vec![spec("x", &[])]
                    },
                    0,
                    1
                )
                .await
                .is_err(),
            "split missing"
        );
        assert!(
            fleet
                .apply_edit(
                    PlanEdit::SplitTask {
                        task_id: "a".into(),
                        into: vec![]
                    },
                    0,
                    1
                )
                .await
                .is_err(),
            "empty split"
        );
        assert!(
            fleet
                .apply_edit(
                    PlanEdit::Retitle {
                        task_id: "ghost".into(),
                        title: "t".into(),
                        detail: "d".into()
                    },
                    0,
                    1
                )
                .await
                .is_err(),
            "retitle missing"
        );

        // No malformed edit touched the durable plan.
        let v = fleet.view().await.unwrap();
        assert_eq!(v.revision, 0);
        assert_eq!(v.generation, 0);
        assert_eq!(v.tasks.len(), 1);
    }

    #[tokio::test]
    async fn view_joins_spec_and_execution_state_with_evidence() {
        let (_d, store) = fresh().await;
        let acceptance = vec![AcceptanceCriterion {
            id: "c1".into(),
            description: "out.txt exists".into(),
            verifier: Verifier::FileExists {
                path: "out.txt".into(),
            },
        }];
        let granted = WorkerGrant {
            network: crate::grant::NetworkGrant::Hosts(vec!["example.com".into()]),
            tools: vec!["read_file".into(), "write_file".into(), "web_fetch".into()],
            fs: crate::grant::FsGrant::default(),
        };
        let task = TaskSpec {
            task_id: "a".into(),
            title: "Build".into(),
            detail: "make out.txt".into(),
            deps: vec![],
            acceptance: acceptance.clone(),
            grant: granted.clone(),
        };
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "ship it",
            vec![task],
            0,
        )
        .await
        .unwrap();

        // Before running: spec joined, no verdict / evidence yet.
        let v = fleet.view().await.unwrap();
        assert_eq!(v.objective, "ship it");
        let tv = &v.tasks[0];
        assert_eq!(tv.title, "Build");
        assert_eq!(tv.detail, "make out.txt");
        assert_eq!(tv.acceptance.len(), 1);
        assert_eq!(
            tv.grant, granted,
            "the operator grant is projected from the plan into the view (PR A)"
        );
        assert_eq!(tv.status, ChildStatus::Ready);
        assert!(tv.verdict.is_none());
        assert!(tv.evidence.is_empty());
        assert!(tv.current_attempt_id.is_none());

        // After Accepted: verdict + evidence surface from the child outcome.
        run_and_record(&fleet, "a", accepted(), 1).await;
        let v = fleet.view().await.unwrap();
        let tv = &v.tasks[0];
        assert_eq!(tv.status, ChildStatus::Succeeded);
        assert!(matches!(
            tv.verdict,
            Some(AcceptanceVerdict::Accepted { .. })
        ));
        assert_eq!(
            tv.evidence.len(),
            1,
            "evidence lives with the outcome, surfaced in the view"
        );
        assert_eq!(tv.evidence[0].locator, "out.txt");
        assert_eq!(tv.acceptance.len(), 1, "the acceptance spec is preserved");
    }

    #[tokio::test]
    async fn summary_counts_tasks_by_state() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"]), spec("c", &["a"])],
            0,
        )
        .await
        .unwrap();

        let s = fleet.summary().await.unwrap();
        assert_eq!(s.total, 3);
        assert_eq!(s.ready, 1);
        assert_eq!(s.planned, 2);
        assert_eq!(s.succeeded, 0);
        assert_eq!(s.status, FleetStatus::Active);

        run_and_record(&fleet, "a", accepted(), 1).await;
        let s = fleet.summary().await.unwrap();
        assert_eq!(s.succeeded, 1);
        assert_eq!(s.ready, 2, "both dependents unblocked");
        assert_eq!(s.planned, 0);
    }

    #[tokio::test]
    async fn bind_attaches_to_an_existing_fleet() {
        let (_d, store) = fresh().await;
        Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();

        // A fresh handle over the same store sees the same durable state.
        let handle = Fleet::bind(store.clone(), "f1");
        assert_eq!(handle.ready_tasks(9).await.unwrap(), vec!["a".to_string()]);
        assert_eq!(handle.view().await.unwrap().objective, "obj");
    }

    #[tokio::test]
    async fn record_outcome_appends_a_complete_decision() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        run_and_record(&fleet, "a", accepted(), 1).await;

        let log = store.list_decisions("f1").await.unwrap();
        assert!(
            log.iter().any(|d| d.kind == DecisionKind::Plan),
            "create logged a Plan"
        );
        assert!(
            log.iter().any(|d| d.kind == DecisionKind::Complete),
            "record_outcome logged a Complete"
        );
    }

    // ---- codex-review findings --------------------------------------------

    /// P1-a: removing a task that a surviving task depends on would leave a
    /// dependent wedged on a nonexistent dep — rejected (dangling), no
    /// mutation.
    #[tokio::test]
    async fn apply_edit_remove_task_with_a_dependent_is_rejected() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"])],
            0,
        )
        .await
        .unwrap();

        let err = fleet
            .apply_edit(PlanEdit::RemoveTask("a".into()), 0, 5)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<PlanGraphError>(),
            Some(PlanGraphError::DanglingDependency { .. })
        ));
        assert_eq!(fleet.view().await.unwrap().revision, 0, "no mutation");
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Ready
        );
    }

    /// P1-a: splitting a task that a `b → a` dep points at leaves `b`
    /// dangling — rejected.
    #[tokio::test]
    async fn apply_edit_split_task_pointed_at_by_a_dep_is_rejected() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"])],
            0,
        )
        .await
        .unwrap();

        let err = fleet
            .apply_edit(
                PlanEdit::SplitTask {
                    task_id: "a".into(),
                    into: vec![spec("a1", &[]), spec("a2", &[])],
                },
                0,
                5,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<PlanGraphError>(),
            Some(PlanGraphError::DanglingDependency { .. })
        ));
        assert_eq!(fleet.view().await.unwrap().revision, 0);
    }

    /// P1-a: a `SplitTask` sub-task reusing the split id (or any existing id)
    /// is rejected — else `replan` would treat the already-existing child as
    /// a survivor of the new spec (false completion).
    #[tokio::test]
    async fn apply_edit_split_task_reusing_an_id_is_rejected() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("big", &[])],
            0,
        )
        .await
        .unwrap();

        let err = fleet
            .apply_edit(
                PlanEdit::SplitTask {
                    task_id: "big".into(),
                    into: vec![spec("big", &[]), spec("p2", &[])],
                },
                0,
                5,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<PlanGraphError>(),
            Some(PlanGraphError::SplitIdReuse(_))
        ));
        assert_eq!(fleet.view().await.unwrap().revision, 0);
        assert_eq!(
            store.get_child("f1", "big").await.unwrap().unwrap().status,
            ChildStatus::Ready,
            "the split id was not touched"
        );
    }

    /// P1-a: self / dangling / cyclic deps are all rejected (typed), and the
    /// plan never enters a cyclic state.
    #[tokio::test]
    async fn apply_edit_rejects_self_dangling_and_cyclic_deps() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[]), spec("b", &[])],
            0,
        )
        .await
        .unwrap();

        let e = fleet
            .apply_edit(
                PlanEdit::RetargetDeps {
                    task_id: "a".into(),
                    deps: vec!["a".into()],
                },
                0,
                5,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            e.downcast_ref::<PlanGraphError>(),
            Some(PlanGraphError::SelfDependency(_))
        ));

        let e = fleet
            .apply_edit(
                PlanEdit::RetargetDeps {
                    task_id: "a".into(),
                    deps: vec!["ghost".into()],
                },
                0,
                5,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            e.downcast_ref::<PlanGraphError>(),
            Some(PlanGraphError::DanglingDependency { .. })
        ));

        // a → b (valid), then b → a would close a cycle → rejected.
        fleet
            .apply_edit(
                PlanEdit::RetargetDeps {
                    task_id: "a".into(),
                    deps: vec!["b".into()],
                },
                0,
                5,
            )
            .await
            .unwrap();
        let cur = fleet.view().await.unwrap().revision;
        let e = fleet
            .apply_edit(
                PlanEdit::RetargetDeps {
                    task_id: "b".into(),
                    deps: vec!["a".into()],
                },
                cur,
                6,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            e.downcast_ref::<PlanGraphError>(),
            Some(PlanGraphError::Cycle(_))
        ));
        assert_eq!(
            fleet.view().await.unwrap().revision,
            cur,
            "cycle not committed"
        );
    }

    /// P1-b: readiness is self-healing. If the promotion after a completion
    /// is missed (here we complete via the store directly, bypassing
    /// `record_outcome`'s `resolve_ready`), `ready_tasks` still returns the
    /// now-eligible successor AND leaves it genuinely `Ready` (launchable).
    #[tokio::test]
    async fn ready_tasks_self_heals_a_missed_promotion() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"])],
            0,
        )
        .await
        .unwrap();

        // Complete `a` via the STORE (no Fleet::record_outcome → no
        // resolve_ready fires), simulating a crash / wake-ordering between the
        // completion commit and the dependent-promotion.
        let attempt = match store
            .launch_child("f1", "a", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        store.mark_running("a", &attempt).await.unwrap();
        store
            .complete_child(
                "f1",
                "a",
                &attempt,
                accepted(),
                ChildResultSnapshot::default(),
                80,
                EPOCH,
                1,
            )
            .await
            .unwrap();
        assert_eq!(
            store.get_child("f1", "b").await.unwrap().unwrap().status,
            ChildStatus::Planned,
            "promotion was missed — b is still Planned in the store"
        );

        // Self-heal: ready_tasks returns b and promotes it to Ready.
        assert_eq!(fleet.ready_tasks(5).await.unwrap(), vec!["b".to_string()]);
        assert_eq!(
            store.get_child("f1", "b").await.unwrap().unwrap().status,
            ChildStatus::Ready,
            "ready_tasks self-healed the promotion"
        );
        // b is genuinely launchable now.
        assert!(matches!(
            store
                .launch_child("f1", "b", 100, 6, EPOCH, TTL)
                .await
                .unwrap(),
            LaunchOutcome::Launched { .. }
        ));
    }

    /// P1-c: `load_snapshot` is internally consistent even under a concurrent
    /// replan stream — the plan revision, fleet generation, and every child's
    /// generation always come from the same instant (a torn read would show a
    /// mismatch). Replan-only edits keep `revision == generation`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_snapshot_is_internally_consistent_under_concurrent_replan() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();

        // Writer: a stream of replan-based edits (add then remove), each of
        // which bumps BOTH revision and generation and re-stamps every child.
        let writer_fleet = fleet.clone();
        let writer = tokio::spawn(async move {
            for i in 0..40u64 {
                let rev = writer_fleet.view().await.unwrap().revision;
                let _ = writer_fleet
                    .apply_edit(
                        PlanEdit::AddTask(spec(&format!("t{i}"), &[])),
                        rev,
                        1_000 + i,
                    )
                    .await;
                let rev = writer_fleet.view().await.unwrap().revision;
                let _ = writer_fleet
                    .apply_edit(PlanEdit::RemoveTask(format!("t{i}")), rev, 2_000 + i)
                    .await;
            }
        });

        // Reader: every snapshot must be internally consistent.
        for _ in 0..200 {
            if let Some(snap) = store.load_snapshot("f1").await.unwrap() {
                if let Some(plan) = &snap.plan {
                    assert_eq!(
                        plan.revision, snap.fleet.generation,
                        "plan revision must match fleet generation in one snapshot"
                    );
                    for c in &snap.children {
                        assert_eq!(
                            c.generation, snap.fleet.generation,
                            "every child's generation must match the fleet's in one snapshot"
                        );
                    }
                }
            }
        }
        writer.await.unwrap();
    }

    /// P2-a: `create` validates the whole graph before any write, so a bad
    /// (here cyclic) graph creates NOTHING.
    #[tokio::test]
    async fn create_with_a_bad_graph_writes_nothing() {
        let (_d, store) = fresh().await;
        let bad = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &["b"]), spec("b", &["a"])],
            0,
        )
        .await;
        assert!(matches!(
            bad.err().unwrap().downcast_ref::<PlanGraphError>(),
            Some(PlanGraphError::Cycle(_))
        ));
        // Nothing durable was written.
        assert!(store.get_fleet("f1").await.unwrap().is_none());
        assert!(store.get_plan("f1").await.unwrap().is_none());
        assert!(store.list_children("f1").await.unwrap().is_empty());
    }

    /// P2-a: a dangling dep in `create` is likewise rejected before any write.
    #[tokio::test]
    async fn create_with_a_dangling_dep_writes_nothing() {
        let (_d, store) = fresh().await;
        let bad = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &["nope"])],
            0,
        )
        .await;
        assert!(bad.is_err());
        assert!(store.get_fleet("f1").await.unwrap().is_none());
    }

    /// P2-b: records are stamped at the CURRENT [`SCHEMA_VERSION`] (so an older
    /// binary drops them via the higher-version guard rather than mis-parsing).
    /// PR B bumped 2 → 3 for the `ChildStatus::Blocked` variant.
    #[tokio::test]
    async fn records_are_written_at_current_schema_version() {
        let (_d, store) = fresh().await;
        Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        assert_eq!(SCHEMA_VERSION, 3);
        assert_eq!(
            store.get_plan("f1").await.unwrap().unwrap().schema_version,
            SCHEMA_VERSION,
        );
        assert_eq!(
            store
                .get_child("f1", "a")
                .await
                .unwrap()
                .unwrap()
                .schema_version,
            SCHEMA_VERSION,
        );
    }

    /// P2-c: `Retitle` is spec-only — it must NOT interrupt a live attempt or
    /// bump the generation. The proof: after retitling a `Running` task, the
    /// SAME attempt still completes (it would be `Superseded` if `replan` had
    /// bumped the generation).
    #[tokio::test]
    async fn retitle_does_not_interrupt_a_running_task() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000),
            "obj",
            vec![spec("a", &[])],
            0,
        )
        .await
        .unwrap();
        let attempt = match store
            .launch_child("f1", "a", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        store.mark_running("a", &attempt).await.unwrap();
        let gen_before = store.get_fleet("f1").await.unwrap().unwrap().generation;

        let out = fleet
            .apply_edit(
                PlanEdit::Retitle {
                    task_id: "a".into(),
                    title: "renamed".into(),
                    detail: "new detail".into(),
                },
                0,
                5,
            )
            .await
            .unwrap();
        assert_eq!(out, PlanMutateOutcome::Mutated { revision: 1 });

        assert_eq!(
            store
                .get_attempt("a", &attempt)
                .await
                .unwrap()
                .unwrap()
                .status,
            AttemptStatus::Running,
            "retitle must not interrupt the live attempt"
        );
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Running
        );
        assert_eq!(
            store.get_fleet("f1").await.unwrap().unwrap().generation,
            gen_before,
            "retitle must not bump the generation"
        );

        let v = fleet.view().await.unwrap();
        assert_eq!(v.revision, 1, "spec-only edit still advances the revision");
        let a = v.tasks.iter().find(|t| t.task_id == "a").unwrap();
        assert_eq!(a.title, "renamed");
        assert_eq!(a.detail, "new detail");

        // The killer assertion: the same attempt still completes (would be
        // Superseded had the generation been bumped).
        assert_eq!(
            fleet
                .record_outcome("a", &attempt, accepted(), 80, EPOCH, 9)
                .await
                .unwrap(),
            CompleteOutcome::Completed
        );
    }

    /// P2-d: retargeting a terminal task's deps is rejected — its spec is
    /// frozen (it would keep its `Succeeded` outcome while dependents run
    /// against a changed prerequisite set).
    #[tokio::test]
    async fn retarget_deps_on_a_terminal_task_is_rejected() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[]), spec("b", &[])],
            0,
        )
        .await
        .unwrap();
        run_and_record(&fleet, "a", accepted(), 1).await;
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Succeeded
        );

        // `a` is terminal; retargeting its deps is rejected. (complete_child
        // does not touch the plan, so the plan revision is still 0.)
        let err = fleet
            .apply_edit(
                PlanEdit::RetargetDeps {
                    task_id: "a".into(),
                    deps: vec!["b".into()],
                },
                0,
                5,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<PlanGraphError>(),
            Some(PlanGraphError::RetargetTerminalTask(_))
        ));
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Succeeded,
            "terminal task untouched"
        );
        assert_eq!(fleet.view().await.unwrap().revision, 0);
    }

    // ---- codex round-2 findings (close the check-then-act races) -----------

    fn durable_plan(fleet: &str, revision: u64, tasks: Vec<TaskSpec>) -> DurablePlan {
        DurablePlan {
            schema_version: SCHEMA_VERSION,
            fleet_id: fleet.into(),
            revision,
            objective: "obj".into(),
            tasks: tasks.into_iter().map(PlanTask::from).collect(),
        }
    }

    /// Round-2 P1 (ready_tasks atomicity): the promote-and-collect happens in
    /// one store txn, so concurrent `ready_tasks` calls racing a completion
    /// never return a bogus task and the successor is never permanently
    /// dropped — after `a` completes, `b` is deterministically `Ready` +
    /// launchable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_ready_tasks_and_completion_never_drop_the_successor() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(1_000_000),
            "obj",
            vec![spec("a", &[]), spec("b", &["a"])],
            0,
        )
        .await
        .unwrap();
        let attempt = match store
            .launch_child("f1", "a", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        store.mark_running("a", &attempt).await.unwrap();

        // Complete `a` via the store (skips the eager resolve), racing pollers.
        let cstore = store.clone();
        let completer = tokio::spawn(async move {
            cstore
                .complete_child(
                    "f1",
                    "a",
                    &attempt,
                    accepted(),
                    ChildResultSnapshot::default(),
                    80,
                    EPOCH,
                    1,
                )
                .await
                .unwrap();
        });
        let mut pollers = Vec::new();
        for _ in 0..8 {
            let pf = fleet.clone();
            pollers.push(tokio::spawn(async move {
                for _ in 0..40 {
                    let r = pf.ready_tasks(2).await.unwrap();
                    assert!(
                        r.is_empty() || r == vec!["b".to_string()],
                        "atomic resolve returned a bogus set: {r:?}"
                    );
                }
            }));
        }
        completer.await.unwrap();
        for p in pollers {
            p.await.unwrap();
        }

        // Deterministic post-condition: `a` is done, so `b` is Ready + launchable.
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Succeeded
        );
        assert_eq!(fleet.ready_tasks(99).await.unwrap(), vec!["b".to_string()]);
        assert_eq!(
            store.get_child("f1", "b").await.unwrap().unwrap().status,
            ChildStatus::Ready
        );
        assert!(matches!(
            store
                .launch_child("f1", "b", 100, 100, EPOCH, TTL)
                .await
                .unwrap(),
            LaunchOutcome::Launched { .. }
        ));
    }

    /// Round-2 P1 (terminal freeze, deterministic): `replan` rejects a
    /// dep-change on a task that is already terminal (`Succeeded` or
    /// `Failed`) INSIDE its write txn, with zero mutation — this is the state
    /// a lost race would present.
    #[tokio::test]
    async fn replan_rejects_a_dep_change_on_a_terminal_task() {
        let (_d, store) = fresh().await;
        let fleet = Fleet::create(
            store.clone(),
            "f1",
            controller(),
            None,
            "default",
            budget(10_000),
            "obj",
            vec![spec("a", &[]), spec("b", &[])],
            0,
        )
        .await
        .unwrap();

        // Succeeded case: a's deps change [] -> [b] with a already Succeeded.
        run_and_record(&fleet, "a", accepted(), 1).await;
        let out = store
            .replan(
                "f1",
                0,
                durable_plan("f1", 0, vec![spec("a", &["b"]), spec("b", &[])]),
                5,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            PlanMutateOutcome::RejectedTerminalDepChange {
                task_id: "a".into()
            }
        );
        let a = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(a.status, ChildStatus::Succeeded);
        assert_eq!(a.deps, Vec::<String>::new(), "terminal deps stayed frozen");
        assert_eq!(
            store.get_plan("f1").await.unwrap().unwrap().revision,
            0,
            "no mutation"
        );
        assert_eq!(store.get_fleet("f1").await.unwrap().unwrap().generation, 0);

        // Failed case: b races to Failed, then a dep-change on b is rejected.
        run_and_record(&fleet, "b", rejected(), 2).await;
        let out = store
            .replan(
                "f1",
                0,
                durable_plan("f1", 0, vec![spec("a", &[]), spec("b", &["a"])]),
                6,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            PlanMutateOutcome::RejectedTerminalDepChange {
                task_id: "b".into()
            }
        );
    }

    /// Round-2 P1 (terminal freeze, race): the out-of-txn pre-check can be
    /// bypassed by a task completing between the snapshot and `replan`, but
    /// the in-txn guard closes it — a task that ends terminal NEVER has its
    /// deps changed. Runs many completion-vs-retarget interleavings.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retarget_deps_racing_a_completion_never_mutates_the_terminal_task() {
        for iter in 0..40u64 {
            let (_d, store) = fresh().await;
            let fid = format!("f{iter}");
            let fleet = Fleet::create(
                store.clone(),
                fid.clone(),
                controller(),
                None,
                "default",
                budget(1_000_000),
                "obj",
                vec![spec("a", &[]), spec("b", &[])],
                0,
            )
            .await
            .unwrap();
            let attempt = match store
                .launch_child(&fid, "a", 100, 0, EPOCH, TTL)
                .await
                .unwrap()
            {
                LaunchOutcome::Launched { attempt_id } => attempt_id,
                other => panic!("{other:?}"),
            };
            store.mark_running("a", &attempt).await.unwrap();

            let cstore = store.clone();
            let cfid = fid.clone();
            let completer = tokio::spawn(async move {
                let _ = cstore
                    .complete_child(
                        &cfid,
                        "a",
                        &attempt,
                        accepted(),
                        ChildResultSnapshot::default(),
                        80,
                        EPOCH,
                        1,
                    )
                    .await;
            });
            let efleet = fleet.clone();
            let editor = tokio::spawn(async move {
                // May succeed (a still Running → interrupt+retarget) or be
                // rejected (a raced to terminal). Either is fine.
                let _ = efleet
                    .apply_edit(
                        PlanEdit::RetargetDeps {
                            task_id: "a".into(),
                            deps: vec!["b".into()],
                        },
                        0,
                        5,
                    )
                    .await;
            });
            completer.await.unwrap();
            editor.await.unwrap();

            // Invariant: if `a` ended terminal, its deps were never changed.
            let a = store.get_child(&fid, "a").await.unwrap().unwrap();
            if a.status.is_terminal() {
                assert_eq!(
                    a.deps,
                    Vec::<String>::new(),
                    "a completed → its deps must stay frozen (iter {iter})"
                );
            }
        }
    }

    /// Round-2 P2: an advisory decision-log append failure must NOT turn a
    /// committed op into an `Err`. `Fleet::note` swallows it (the state
    /// machine is load-bearing, the audit log is not).
    #[tokio::test]
    async fn a_decision_log_failure_does_not_fail_the_operation() {
        let (_d, store) = fresh().await;
        // A control-char id makes the raw append itself fail...
        assert!(
            store
                .append_decision("bad\u{0}id", "keeper", DecisionKind::Note, "x", 1)
                .await
                .is_err()
        );
        // ...but the best-effort Fleet::note swallows it — reaching the next
        // line proves no panic / no propagation.
        let handle = Fleet::bind(store.clone(), "bad\u{0}id");
        handle.note(DecisionKind::Note, "x", 1).await;
        assert_eq!(
            handle.fleet_id(),
            "bad\u{0}id",
            "handle still usable after a swallowed log failure"
        );
    }
}
