//! `FleetKernelStore` — the durable transactional core of the fleet
//! kernel.
//!
//! One redb database (`fleet-kernel.redb`) holds every fleet's records.
//! **Every** state transition is a single `begin_write` transaction that
//! reads the current row(s), checks a CAS predicate (status / generation
//! / lease / revision), and writes the next state together with the
//! budget settlement and the outbox append — atomically, with no
//! cross-store window (spec §1, §6).
//!
//! ## Cancellation-safety (`io_gate`)
//!
//! redb is synchronous, so all work runs inside `spawn_blocking`. A
//! caller future cancelled mid-`await` (e.g. a dropped keeper turn)
//! cannot abort the already-scheduled blocking closure — so its write
//! could land *after* the next caller's read. To order that, **every**
//! op — reads *and* writes (spec §1 v1.1: reads must gate too) — takes
//! `io_gate.lock_owned().await` and **moves the owned guard into the
//! blocking closure**. The gate is acquired exactly once per public call
//! and released when that call's single `spawn_blocking` returns; no op
//! calls another gated op while holding it, so the design is
//! deadlock-free.
//!
//! ## Key safety
//!
//! Composite keys are `\0`-delimited. Every persisted key component
//! (`fleet_id` / `child_id` / `attempt_id` / `task_id` / dep) is checked
//! by [`key_component_is_safe`] at the entry point — non-empty and free
//! of control/NUL characters — so two different `(fleet, child)` pairs
//! can never encode to the same key (P1-4). Lookups additionally verify
//! the decoded record's own ids against the requested ones (defense in
//! depth).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr, bail};
use octos_core::SessionKey;
use redb::{Database, ReadableTable, TableDefinition};
use uuid::Uuid;

use crate::grant::WorkerGrant;
use crate::records::*;

const FLEETS: TableDefinition<&str, &str> = TableDefinition::new("fleets");
const FLEET_CHILDREN: TableDefinition<&str, &str> = TableDefinition::new("fleet_children");
const ATTEMPTS: TableDefinition<&str, &str> = TableDefinition::new("attempts");
const PLANS: TableDefinition<&str, &str> = TableDefinition::new("plans");
const DECISION_LOG: TableDefinition<&str, &str> = TableDefinition::new("decision_log");
const OUTBOX: TableDefinition<&str, &str> = TableDefinition::new("outbox");

const DB_FILENAME: &str = "fleet-kernel.redb";

// ---------------------------------------------------------------------------
// Outcome / report types
// ---------------------------------------------------------------------------

/// Result of a launch CAS. The three rejections are *values*, not
/// errors: they are ordinary control flow the keeper reasons over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchOutcome {
    Launched { attempt_id: String },
    RejectedNotReady,
    RejectedDoubleLaunch,
    RejectedBudgetExceeded,
}

/// Result of a complete CAS. `Superseded` means the four-part predicate
/// failed (a stale/late attempt); its result is dropped and no state
/// changes — deliberately not an error (spec §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteOutcome {
    Completed,
    Superseded,
}

/// Result of [`FleetKernelStore::deny_escalation`]. Carries the settle outcome
/// AND — computed IN THE SAME write-txn, over the durable post-deny child states
/// — whether the fleet can no longer auto-complete. PR B (codex round-4, defect
/// 2): the keeper drives the goal terminal from `fleet_un_completable` DIRECTLY,
/// so the resolution can never be skipped by a separate fallible read after the
/// durable deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenyEscalationOutcome {
    /// `Completed` when the `Blocked → Failed` transition committed; `Superseded`
    /// when the child was not `Blocked` (an inert no-op deny).
    pub settled: CompleteOutcome,
    /// Whether the fleet is now un-completable — no task can drive it to
    /// all-`Succeeded` because a `Failed` task strands it (mirrors the goal
    /// snapshot's `!complete && any-failed` rule, computed on the durable state).
    /// `false` on a `Superseded` no-op (nothing changed).
    pub fleet_un_completable: bool,
}

/// Result of a `mark_running` CAS. `Superseded` means the identity/predicate
/// fence failed — a genuine lost race (the attempt is stale/superseded or not
/// the child's current attempt), so nothing changes; deliberately **not** an
/// error, exactly like [`CompleteOutcome::Superseded`]. `Err` is reserved for a
/// real infra failure (redb read/commit/join or a corrupt-row parse), which
/// leaves it ambiguous whether the still-`Launching` attempt is ours — the
/// caller must NOT treat that as "not ours".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkRunningOutcome {
    Running,
    Superseded,
}

/// Result of a revision-fenced plan operation (`replan` / `retitle_task`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanMutateOutcome {
    Mutated {
        revision: u64,
    },
    RevisionMismatch {
        actual: u64,
    },
    /// A `RetargetDeps`-style edit tried to change the dependency set of a
    /// task that is terminal (`Succeeded`/`Failed`) — detected **inside**
    /// the `replan` write-txn, so a task that completes between a caller's
    /// out-of-txn pre-check and the CAS cannot slip a dep-change past the
    /// freeze (round-2 P1). No mutation.
    RejectedTerminalDepChange {
        task_id: String,
    },
    /// PR B — a `SetGrant` (grant-widen + resume) targeted a child that is NOT
    /// `Blocked` — detected **inside** the `set_task_grant` write-txn, so a
    /// child that a concurrent `deny_escalation` moved `Blocked → Failed` (or
    /// that a prior grant already resumed) between the caller's out-of-txn read
    /// and the CAS cannot have the denied/stale grant applied. No mutation: the
    /// grant is REFUSED (grant and deny are mutually exclusive).
    RejectedNotBlocked {
        task_id: String,
    },
}

/// Result of an outbox `ack`. `StaleClaim` means the presented
/// `(consumer, claim_token)` did not match the row's current claim — the
/// event was reclaimed by another consumer, so this ack is ignored
/// (P1-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    Acked,
    StaleClaim,
}

/// What a boot reconciliation reclaimed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub interrupted: Vec<InterruptedAttempt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedAttempt {
    pub fleet_id: String,
    pub child_id: String,
    pub attempt_id: String,
}

/// A point-in-time, internally-consistent read of one fleet: its
/// [`FleetRecord`], its [`DurablePlan`] (if any), and all its children —
/// captured under a **single** read transaction (one `io_gate`
/// acquisition), so a concurrent `replan` interleaving between the
/// component reads can never produce an old-plan + new-children mix (P1-c).
/// `children` is sorted by `child_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetSnapshot {
    pub fleet: FleetRecord,
    pub plan: Option<DurablePlan>,
    pub children: Vec<FleetChildRecord>,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Redb-backed fleet kernel store. Cheaply cloneable (`Arc` internals);
/// the `io_gate` serialises all DB access for cancellation-safety.
///
/// `Debug` is derived so the store can be embedded in `Debug`-deriving
/// runtime-state structs (e.g. the server's `AutonomyRuntimeState`); redb's
/// `Database` prints an opaque summary, exposing no row contents.
#[derive(Clone, Debug)]
pub struct FleetKernelStore {
    db: Arc<Database>,
    path: Arc<PathBuf>,
    io_gate: Arc<tokio::sync::Mutex<()>>,
}

impl FleetKernelStore {
    /// Open (or create) the store under `dir`, creating every table.
    pub async fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&dir)
            .await
            .wrap_err("create fleet kernel dir")?;
        let db_path = dir.join(DB_FILENAME);
        let path_for_spawn = db_path.clone();
        let db = tokio::task::spawn_blocking(move || -> Result<Database> {
            let db = Database::create(&path_for_spawn).wrap_err("open fleet-kernel redb")?;
            let wtx = db.begin_write().wrap_err("begin fleet-kernel init")?;
            {
                wtx.open_table(FLEETS)?;
                wtx.open_table(FLEET_CHILDREN)?;
                wtx.open_table(ATTEMPTS)?;
                wtx.open_table(PLANS)?;
                wtx.open_table(DECISION_LOG)?;
                wtx.open_table(OUTBOX)?;
            }
            wtx.commit().wrap_err("commit fleet-kernel init")?;
            Ok(db)
        })
        .await
        .wrap_err("join fleet-kernel open")??;

        Ok(Self {
            db: Arc::new(db),
            path: Arc::new(db_path),
            io_gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Path to the underlying redb file.
    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    // ---- creation helpers -------------------------------------------------

    /// Create a fresh fleet (generation 0, `Active`, soft/hard budget).
    /// Rejects a duplicate `fleet_id` rather than clobbering live state.
    #[allow(clippy::too_many_arguments)] // fleet identity + controller + budget columns are irreducible here
    pub async fn create_fleet(
        &self,
        fleet_id: &str,
        controller_session_key: SessionKey,
        controller_workspace_root: Option<String>,
        profile_id: &str,
        token_budget: u64,
        hard: bool,
        now_ms: u64,
    ) -> Result<()> {
        ensure_key_safe(&[fleet_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let profile_id = profile_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _held = held;
            let wtx = db.begin_write()?;
            {
                let mut fleets = wtx.open_table(FLEETS)?;
                if fleets.get(fleet_id.as_str())?.is_some() {
                    bail!("fleet {fleet_id} already exists");
                }
                let record = FleetRecord {
                    schema_version: SCHEMA_VERSION,
                    fleet_id: fleet_id.clone(),
                    controller_session_key,
                    controller_workspace_root,
                    controller_workspace_has_runtime_hint: None,
                    profile_id,
                    budget: FleetBudget {
                        token_budget,
                        tokens_reserved: 0,
                        tokens_committed: 0,
                        hard,
                    },
                    status: FleetStatus::Active,
                    generation: 0,
                    created_at_ms: now_ms,
                    updated_at_ms: now_ms,
                };
                fleets.insert(fleet_id.as_str(), serde_json::to_string(&record)?.as_str())?;
            }
            wtx.commit()?;
            Ok(())
        })
        .await
        .wrap_err("join create_fleet")?
    }

    /// Create the durable plan for a fleet. **Insert-only** (P1-2): it
    /// refuses to overwrite an existing plan (use [`FleetKernelStore::replan`]
    /// for edits) — a blind overwrite would silently drop tasks without
    /// advancing the generation fence.
    pub async fn create_plan(&self, mut plan: DurablePlan) -> Result<()> {
        ensure_key_safe(&[plan.fleet_id.as_str()])?;
        for t in &plan.tasks {
            ensure_key_safe(&[t.task_id.as_str()])?;
            for d in &t.deps {
                ensure_key_safe(&[d.as_str()])?;
            }
        }
        plan.schema_version = SCHEMA_VERSION;
        let db = self.db.clone();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _held = held;
            let wtx = db.begin_write()?;
            {
                let mut plans = wtx.open_table(PLANS)?;
                if plans.get(plan.fleet_id.as_str())?.is_some() {
                    bail!(
                        "plan for fleet {} already exists; use replan",
                        plan.fleet_id
                    );
                }
                plans.insert(
                    plan.fleet_id.as_str(),
                    serde_json::to_string(&plan)?.as_str(),
                )?;
            }
            wtx.commit()?;
            Ok(())
        })
        .await
        .wrap_err("join create_plan")?
    }

    /// Atomically create a fleet, its durable plan, and one child per plan
    /// task, in a **single** write transaction (P2-a). Either every row is
    /// written or none is — there is no half-created fleet-without-plan on a
    /// mid-sequence failure (unlike composing separate `create_fleet`,
    /// `add_child`, and `create_plan` calls). Rejects a duplicate fleet or
    /// plan. A dep-free task starts `Ready`, a dep-gated one `Planned`
    /// (nothing is `Succeeded` at genesis). Callers should validate the task
    /// graph (unique ids, no dangling / self / cyclic deps) **before**
    /// calling this; the store only enforces key-safety and duplicate
    /// rejection.
    #[allow(clippy::too_many_arguments)] // fleet identity + controller + budget + plan inputs are irreducible here
    pub async fn create_fleet_with_plan(
        &self,
        controller_session_key: SessionKey,
        controller_workspace_root: Option<String>,
        profile_id: &str,
        token_budget: u64,
        hard: bool,
        plan: DurablePlan,
        now_ms: u64,
    ) -> Result<()> {
        self.create_fleet_with_plan_and_workspace_provenance(
            controller_session_key,
            controller_workspace_root,
            None,
            profile_id,
            token_budget,
            hard,
            plan,
            now_ms,
        )
        .await
    }

    /// Like [`Self::create_fleet_with_plan`], while also persisting whether the
    /// controller root is a genuine runtime cwd hint. New callers should pass
    /// `Some(true|false)`; `None` is reserved for legacy/unknown provenance.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create_fleet_with_plan_and_workspace_provenance(
        &self,
        controller_session_key: SessionKey,
        controller_workspace_root: Option<String>,
        controller_workspace_has_runtime_hint: Option<bool>,
        profile_id: &str,
        token_budget: u64,
        hard: bool,
        plan: DurablePlan,
        now_ms: u64,
    ) -> Result<()> {
        let fleet_id = plan.fleet_id.clone();
        ensure_key_safe(&[fleet_id.as_str()])?;
        for t in &plan.tasks {
            ensure_key_safe(&[t.task_id.as_str()])?;
            for d in &t.deps {
                ensure_key_safe(&[d.as_str()])?;
            }
        }
        let db = self.db.clone();
        let profile_id = profile_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _held = held;
            let mut plan = plan;
            plan.schema_version = SCHEMA_VERSION;
            let wtx = db.begin_write()?;
            {
                let mut fleets = wtx.open_table(FLEETS)?;
                let mut plans = wtx.open_table(PLANS)?;
                let mut children = wtx.open_table(FLEET_CHILDREN)?;

                if fleets.get(fleet_id.as_str())?.is_some() {
                    bail!("fleet {fleet_id} already exists");
                }
                if plans.get(fleet_id.as_str())?.is_some() {
                    bail!("plan for fleet {fleet_id} already exists");
                }

                let fleet = FleetRecord {
                    schema_version: SCHEMA_VERSION,
                    fleet_id: fleet_id.clone(),
                    controller_session_key,
                    controller_workspace_root,
                    controller_workspace_has_runtime_hint,
                    profile_id,
                    budget: FleetBudget {
                        token_budget,
                        tokens_reserved: 0,
                        tokens_committed: 0,
                        hard,
                    },
                    status: FleetStatus::Active,
                    generation: 0,
                    created_at_ms: now_ms,
                    updated_at_ms: now_ms,
                };
                fleets.insert(fleet_id.as_str(), serde_json::to_string(&fleet)?.as_str())?;

                for t in &plan.tasks {
                    let ckey = child_key(&fleet_id, &t.task_id);
                    if children.get(ckey.as_str())?.is_some() {
                        bail!("duplicate child {} in fleet {fleet_id}", t.task_id);
                    }
                    let status = if t.deps.is_empty() {
                        ChildStatus::Ready
                    } else {
                        ChildStatus::Planned
                    };
                    let record = FleetChildRecord {
                        schema_version: SCHEMA_VERSION,
                        fleet_id: fleet_id.clone(),
                        child_id: t.task_id.clone(),
                        worker_kind: WorkerKind::StatelessTask,
                        status,
                        current_attempt_id: None,
                        attempts_used: 0,
                        outcome: None,
                        tokens_committed: 0,
                        deps: t.deps.clone(),
                        pending_escalation: None,
                        generation: 0,
                        updated_at_ms: now_ms,
                    };
                    children.insert(ckey.as_str(), serde_json::to_string(&record)?.as_str())?;
                }

                plans.insert(fleet_id.as_str(), serde_json::to_string(&plan)?.as_str())?;
            }
            wtx.commit()?;
            Ok(())
        })
        .await
        .wrap_err("join create_fleet_with_plan")?
    }

    /// Add a child (== plan task) to a fleet. `deps` are the task-ids
    /// that must be `Succeeded` first; an empty `deps` makes the child
    /// `Ready` immediately, otherwise it starts `Planned` and is
    /// promoted by [`FleetKernelStore::mark_ready`]. Rejects a duplicate
    /// child.
    pub async fn add_child(
        &self,
        fleet_id: &str,
        child_id: &str,
        deps: Vec<String>,
        now_ms: u64,
    ) -> Result<()> {
        ensure_key_safe(&[fleet_id, child_id])?;
        for d in &deps {
            ensure_key_safe(&[d.as_str()])?;
        }
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _held = held;
            let wtx = db.begin_write()?;
            {
                let fleets = wtx.open_table(FLEETS)?;
                let mut children = wtx.open_table(FLEET_CHILDREN)?;

                let Some(fj) = fleets
                    .get(fleet_id.as_str())?
                    .map(|g| g.value().to_string())
                else {
                    bail!("cannot add child to unknown fleet {fleet_id}");
                };
                let Some(fleet) = decode_row::<FleetRecord>(&fj)? else {
                    bail!("fleet {fleet_id} is a newer schema");
                };

                let ckey = child_key(&fleet_id, &child_id);
                if children.get(ckey.as_str())?.is_some() {
                    bail!("child {child_id} already exists in fleet {fleet_id}");
                }

                let status = if deps.is_empty() {
                    ChildStatus::Ready
                } else {
                    ChildStatus::Planned
                };
                let record = FleetChildRecord {
                    schema_version: SCHEMA_VERSION,
                    fleet_id: fleet_id.clone(),
                    child_id: child_id.clone(),
                    worker_kind: WorkerKind::StatelessTask,
                    status,
                    current_attempt_id: None,
                    attempts_used: 0,
                    outcome: None,
                    tokens_committed: 0,
                    deps,
                    pending_escalation: None,
                    generation: fleet.generation,
                    updated_at_ms: now_ms,
                };
                children.insert(ckey.as_str(), serde_json::to_string(&record)?.as_str())?;
            }
            wtx.commit()?;
            Ok(())
        })
        .await
        .wrap_err("join add_child")?
    }

    /// Re-evaluate a `Planned` child's readiness: if every dependency
    /// child is `Succeeded`, promote it to `Ready` and return `true`.
    /// A no-op (returns `false`) for a child that is not `Planned` or
    /// whose deps are not all met.
    pub async fn mark_ready(&self, fleet_id: &str, child_id: &str, now_ms: u64) -> Result<bool> {
        ensure_key_safe(&[fleet_id, child_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let _held = held;
            let wtx = db.begin_write()?;
            let promoted = {
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let ckey = child_key(&fleet_id, &child_id);
                let Some(cj) = children.get(ckey.as_str())?.map(|g| g.value().to_string()) else {
                    bail!("mark_ready: unknown child {child_id} in fleet {fleet_id}");
                };
                let Some(mut child) = decode_row::<FleetChildRecord>(&cj)? else {
                    bail!("child {child_id} is a newer schema");
                };
                // Defense in depth (P1-4): the decoded row must own the key.
                if child.fleet_id != fleet_id || child.child_id != child_id {
                    bail!("mark_ready: child identity mismatch");
                }
                if child.status != ChildStatus::Planned {
                    false
                } else {
                    let deps = child.deps.clone();
                    let mut all_met = true;
                    for dep in &deps {
                        let dkey = child_key(&fleet_id, dep);
                        // A dep counts only if its own ids match the key it
                        // was read under AND it Succeeded (don't trust a
                        // tampered/corrupt dependency row).
                        let dep_ok =
                            match children.get(dkey.as_str())?.map(|g| g.value().to_string()) {
                                Some(dj) => decode_row::<FleetChildRecord>(&dj)?
                                    .map(|d| {
                                        d.fleet_id == fleet_id
                                            && d.child_id.as_str() == dep.as_str()
                                            && d.status == ChildStatus::Succeeded
                                    })
                                    .unwrap_or(false),
                                None => false,
                            };
                        if !dep_ok {
                            all_met = false;
                            break;
                        }
                    }
                    if all_met {
                        child.status = ChildStatus::Ready;
                        child.updated_at_ms = now_ms;
                        children.insert(ckey.as_str(), serde_json::to_string(&child)?.as_str())?;
                        true
                    } else {
                        false
                    }
                }
            };
            if promoted {
                wtx.commit()?;
            }
            Ok(promoted)
        })
        .await
        .wrap_err("join mark_ready")?
    }

    /// Atomically resolve readiness and collect the launchable set, in
    /// **one** write-txn (round-2 P1). Inside a single `begin_write`: read
    /// every child of the fleet, promote each `Planned` child whose deps are
    /// **all** `Succeeded` to `Ready`, then collect every `Ready` child with
    /// no live attempt; commit; return the collected ids (sorted).
    ///
    /// Doing the promote-decision and the collect in the same transaction is
    /// what makes readiness self-healing without a TOCTOU: a `snapshot →
    /// mark_ready → re-snapshot` sequence could tear (a completion landing
    /// between the two reads leaves a successor neither promoted nor
    /// collected). Here they cannot. Only all-`Succeeded` deps promote; a
    /// `Failed`/`Cancelled` dep keeps a child `Planned`.
    pub async fn resolve_and_collect_ready(
        &self,
        fleet_id: &str,
        now_ms: u64,
    ) -> Result<Vec<String>> {
        ensure_key_safe(&[fleet_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let _held = held;
            let wtx = db.begin_write()?;
            let mut ready = Vec::new();
            {
                let mut children = wtx.open_table(FLEET_CHILDREN)?;

                // Collect this fleet's children (owned, identity-checked) so
                // the iterator's borrow is released before we mutate.
                let recs: Vec<FleetChildRecord> = {
                    let mut out = Vec::new();
                    for item in children.iter()? {
                        let (k, v) = item?;
                        if let Some(rec) = decode_row::<FleetChildRecord>(v.value())? {
                            if rec.fleet_id == fleet_id
                                && child_key(&rec.fleet_id, &rec.child_id) == k.value()
                            {
                                out.push(rec);
                            }
                        }
                    }
                    out
                };

                // Succeeded set from this single consistent read (promotion
                // to `Ready` never adds to it, so one pass suffices).
                let succeeded: HashSet<String> = recs
                    .iter()
                    .filter(|c| c.status == ChildStatus::Succeeded)
                    .map(|c| c.child_id.clone())
                    .collect();

                for mut c in recs {
                    if c.status == ChildStatus::Planned
                        && c.current_attempt_id.is_none()
                        && c.deps.iter().all(|d| succeeded.contains(d))
                    {
                        c.status = ChildStatus::Ready;
                        c.updated_at_ms = now_ms;
                        let ckey = child_key(&fleet_id, &c.child_id);
                        children.insert(ckey.as_str(), serde_json::to_string(&c)?.as_str())?;
                    }
                    if c.status == ChildStatus::Ready && c.current_attempt_id.is_none() {
                        ready.push(c.child_id.clone());
                    }
                }
            }
            wtx.commit()?;
            ready.sort();
            Ok(ready)
        })
        .await
        .wrap_err("join resolve_and_collect_ready")?
    }

    /// Every **live** fleet (`Active`/`Draining`) that currently has at least
    /// one launchable child. This is the boot-resume driver: a fleet
    /// interrupted by a restart has its in-flight children reset to `Ready` by
    /// [`Self::reconcile`], but reconcile emits NO outbox event, so the outbox
    /// consumer never wakes the keeper and nothing re-dispatches. This query
    /// finds exactly the fleets whose keeper must be re-woken at boot.
    ///
    /// Two-phase to respect the non-reentrant `io_gate`: phase 1 takes the gate
    /// ONCE to snapshot the live fleet records from the `FLEETS` table
    /// (mirroring [`Self::list_children`]'s iterator) and then RELEASES it;
    /// phase 2 calls [`Self::resolve_and_collect_ready`] per candidate — each
    /// re-acquires the gate for its own one-write-txn, so it MUST run outside
    /// phase 1's guard. Reusing `resolve_and_collect_ready` rather than a naive
    /// `status == Ready` scan is deliberate: it heals a missed `Planned → Ready`
    /// promotion in-txn (a dep that `Succeeded` pre-crash whose dependent never
    /// promoted), so a fleet stranded in exactly that state is still detected.
    /// A candidate whose ready set comes back empty (all children terminal or
    /// in-flight) is dropped; terminal fleets (`Complete`/`Failed`/`Cancelled`)
    /// are excluded up front by the status filter.
    pub async fn fleets_with_ready_children(&self, now_ms: u64) -> Result<Vec<FleetRecord>> {
        // Phase 1 (one io_gate acquisition): snapshot the live fleet records.
        let candidates: Vec<FleetRecord> = {
            let db = self.db.clone();
            let held = self.io_gate.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || -> Result<Vec<FleetRecord>> {
                let _held = held;
                let rtx = db.begin_read()?;
                let table = rtx.open_table(FLEETS)?;
                let mut out = Vec::new();
                for item in table.iter()? {
                    let (k, v) = item?;
                    if let Some(rec) = decode_row::<FleetRecord>(v.value())? {
                        // Identity check (defense in depth, mirrors get_fleet):
                        // the row must own the key it lives under. Filter to the
                        // two live states — terminal fleets never re-dispatch.
                        if rec.fleet_id == k.value()
                            && matches!(rec.status, FleetStatus::Active | FleetStatus::Draining)
                        {
                            out.push(rec);
                        }
                    }
                }
                Ok(out)
            })
            .await
            .wrap_err("join fleets_with_ready_children")??
        };

        // Phase 2 (gate released): resolve+collect each candidate's ready set.
        // Each call re-acquires the io_gate for its own write-txn, so it runs
        // OUTSIDE phase 1's guard (the Mutex is non-reentrant). Non-empty ⇒ the
        // fleet has launchable work and its keeper needs a wake.
        let mut out = Vec::new();
        for rec in candidates {
            let ready = self
                .resolve_and_collect_ready(&rec.fleet_id, now_ms)
                .await?;
            if !ready.is_empty() {
                out.push(rec);
            }
        }
        Ok(out)
    }

    // ---- CAS state transitions -------------------------------------------

    /// Launch a `Ready` child: one write-txn that checks the predicate
    /// (`Ready` && no live attempt && budget admits) then writes the
    /// child `Launching`, a fresh `Leased` [`Attempt`] (generation
    /// stamped from the fleet), the budget reservation, and a
    /// `ChildLaunching` outbox event — atomically. On any rejection the
    /// child is left untouched (a budget reject does **not** leave it
    /// `Launching`).
    pub async fn launch_child(
        &self,
        fleet_id: &str,
        child_id: &str,
        projected_tokens: u64,
        now_ms: u64,
        owner_epoch: u64,
        lease_ttl_ms: u64,
    ) -> Result<LaunchOutcome> {
        ensure_key_safe(&[fleet_id, child_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let attempt_id = Uuid::new_v4().to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<LaunchOutcome> {
            let _held = held;
            let wtx = db.begin_write()?;
            let outcome = {
                let mut fleets = wtx.open_table(FLEETS)?;
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let mut attempts = wtx.open_table(ATTEMPTS)?;
                let mut outbox = wtx.open_table(OUTBOX)?;

                let ckey = child_key(&fleet_id, &child_id);
                let Some(cj) = children.get(ckey.as_str())?.map(|g| g.value().to_string()) else {
                    return Ok(LaunchOutcome::RejectedNotReady);
                };
                let Some(mut child) = decode_row::<FleetChildRecord>(&cj)? else {
                    return Ok(LaunchOutcome::RejectedNotReady);
                };
                // Defense in depth: the decoded row must own the ids we
                // addressed it by (P1-4).
                if child.fleet_id != fleet_id || child.child_id != child_id {
                    return Ok(LaunchOutcome::RejectedNotReady);
                }

                // A child already in flight (Launching/Running with a live
                // attempt) is a double-launch; anything not Ready is
                // not-ready.
                if child.status != ChildStatus::Ready {
                    if matches!(child.status, ChildStatus::Launching | ChildStatus::Running)
                        && child.current_attempt_id.is_some()
                    {
                        return Ok(LaunchOutcome::RejectedDoubleLaunch);
                    }
                    return Ok(LaunchOutcome::RejectedNotReady);
                }
                if child.current_attempt_id.is_some() {
                    return Ok(LaunchOutcome::RejectedDoubleLaunch);
                }

                let Some(fj) = fleets
                    .get(fleet_id.as_str())?
                    .map(|g| g.value().to_string())
                else {
                    return Ok(LaunchOutcome::RejectedNotReady);
                };
                let Some(mut fleet) = decode_row::<FleetRecord>(&fj)? else {
                    return Ok(LaunchOutcome::RejectedNotReady);
                };
                if fleet.fleet_id != fleet_id {
                    return Ok(LaunchOutcome::RejectedNotReady);
                }

                // Budget predicate — checked *before* any write, so a
                // reject cannot leave a `Launching` child with no worker.
                if !fleet.budget.admits(projected_tokens) {
                    return Ok(LaunchOutcome::RejectedBudgetExceeded);
                }

                // ---- all predicates pass: mutate every table in this txn ----
                // All checked (P2-5): `admits` guaranteed the reserve fits,
                // but compute defensively; the lease expiry and attempt
                // counter must not silently saturate either. Computed before
                // any write so an overflow aborts cleanly.
                let new_reserved = fleet
                    .budget
                    .tokens_reserved
                    .checked_add(projected_tokens)
                    .ok_or_else(|| eyre::eyre!("reservation overflow launching {child_id}"))?;
                let lease_expires = now_ms
                    .checked_add(lease_ttl_ms)
                    .ok_or_else(|| eyre::eyre!("lease-expiry overflow launching {child_id}"))?;
                let attempts_used = child
                    .attempts_used
                    .checked_add(1)
                    .ok_or_else(|| eyre::eyre!("attempts_used overflow for {child_id}"))?;

                child.status = ChildStatus::Launching;
                child.current_attempt_id = Some(attempt_id.clone());
                child.attempts_used = attempts_used;
                child.updated_at_ms = now_ms;
                children.insert(ckey.as_str(), serde_json::to_string(&child)?.as_str())?;

                let attempt = Attempt {
                    schema_version: SCHEMA_VERSION,
                    fleet_id: fleet_id.clone(),
                    child_id: child_id.clone(),
                    attempt_id: attempt_id.clone(),
                    generation: fleet.generation,
                    status: AttemptStatus::Leased,
                    lease: Lease {
                        owner_epoch,
                        expires_at_ms: lease_expires,
                    },
                    reserved_tokens: projected_tokens,
                    result_snapshot: None,
                    started_at_ms: now_ms,
                    ended_at_ms: None,
                };
                let akey = attempt_key(&child_id, &attempt_id);
                attempts.insert(akey.as_str(), serde_json::to_string(&attempt)?.as_str())?;

                fleet.budget.tokens_reserved = new_reserved;
                fleet.updated_at_ms = now_ms;
                fleets.insert(fleet_id.as_str(), serde_json::to_string(&fleet)?.as_str())?;

                append_outbox(
                    &mut outbox,
                    outbox_event(
                        FleetEventKind::ChildLaunching,
                        &fleet_id,
                        Some(&child_id),
                        Some(&attempt_id),
                    ),
                )?;

                LaunchOutcome::Launched {
                    attempt_id: attempt_id.clone(),
                }
            };
            if matches!(outcome, LaunchOutcome::Launched { .. }) {
                wtx.commit()?;
            }
            Ok(outcome)
        })
        .await
        .wrap_err("join launch_child")?
    }

    /// CAS a leased attempt to `Running` (`Leased → Running`) **and** its
    /// child `Launching → Running`, in one write-txn (P2-6). It validates
    /// the whole identity + fence — the attempt belongs to `child_id`, the
    /// child's `current_attempt_id` is this attempt, the child is
    /// `Launching`, the attempt is `Leased`, and `attempt.generation ==
    /// fleet.generation` — **before writing either row**, so any mismatch
    /// (including a pre-replan attempt whose generation is now stale) is
    /// rejected with zero mutation.
    ///
    /// Returns the typed [`MarkRunningOutcome`] (mirrors [`CompleteOutcome`]):
    /// an identity/existence/predicate miss (a genuine lost race — the attempt
    /// is superseded or not the child's current one) is
    /// [`MarkRunningOutcome::Superseded`], **not** an `Err`. `Err` is reserved
    /// for a real infra failure (redb read/commit/join, or a corrupt-row parse
    /// surfaced by `?`) — which leaves the still-`Launching` attempt possibly
    /// ours, so the caller must not disarm its cleanup on that path.
    pub async fn mark_running(
        &self,
        child_id: &str,
        attempt_id: &str,
    ) -> Result<MarkRunningOutcome> {
        ensure_key_safe(&[child_id, attempt_id])?;
        let db = self.db.clone();
        let child_id = child_id.to_string();
        let attempt_id = attempt_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<MarkRunningOutcome> {
            let _held = held;
            let wtx = db.begin_write()?;
            let outcome = {
                let mut attempts = wtx.open_table(ATTEMPTS)?;
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let fleets = wtx.open_table(FLEETS)?;

                // Read the attempt (owned). A missing row / newer-schema row /
                // identity mismatch is a superseded-or-foreign attempt, never an
                // infra error — mirror `complete_child` (only `?` yields `Err`).
                let akey = attempt_key(&child_id, &attempt_id);
                let Some(aj) = attempts.get(akey.as_str())?.map(|g| g.value().to_string()) else {
                    return Ok(MarkRunningOutcome::Superseded);
                };
                let Some(mut attempt) = decode_row::<Attempt>(&aj)? else {
                    return Ok(MarkRunningOutcome::Superseded);
                };
                if attempt.child_id != child_id || attempt.attempt_id != attempt_id {
                    return Ok(MarkRunningOutcome::Superseded);
                }

                // Read the child derived from the attempt's fleet (P1-4).
                let ckey = child_key(&attempt.fleet_id, &child_id);
                let Some(cj) = children.get(ckey.as_str())?.map(|g| g.value().to_string()) else {
                    return Ok(MarkRunningOutcome::Superseded);
                };
                let Some(mut child) = decode_row::<FleetChildRecord>(&cj)? else {
                    return Ok(MarkRunningOutcome::Superseded);
                };
                if child.fleet_id != attempt.fleet_id || child.child_id != child_id {
                    return Ok(MarkRunningOutcome::Superseded);
                }

                // Read the fleet for the generation fence.
                let Some(fj) = fleets
                    .get(attempt.fleet_id.as_str())?
                    .map(|g| g.value().to_string())
                else {
                    return Ok(MarkRunningOutcome::Superseded);
                };
                let Some(fleet) = decode_row::<FleetRecord>(&fj)? else {
                    return Ok(MarkRunningOutcome::Superseded);
                };
                if fleet.fleet_id != attempt.fleet_id {
                    return Ok(MarkRunningOutcome::Superseded);
                }

                // Validate EVERYTHING before writing either row (P2-6). Each is
                // a CAS-predicate fence — a miss is a lost race (`Superseded`).
                if child.current_attempt_id.as_deref() != Some(attempt_id.as_str())
                    || child.status != ChildStatus::Launching
                    || attempt.status != AttemptStatus::Leased
                    || attempt.generation != fleet.generation
                {
                    return Ok(MarkRunningOutcome::Superseded);
                }

                // All checks passed — write both rows.
                attempt.status = AttemptStatus::Running;
                attempts.insert(akey.as_str(), serde_json::to_string(&attempt)?.as_str())?;
                child.status = ChildStatus::Running;
                children.insert(ckey.as_str(), serde_json::to_string(&child)?.as_str())?;

                MarkRunningOutcome::Running
            };
            // Only a real state change commits; a `Superseded` returned above
            // never reaches here (it exits the closure, dropping `wtx` unwritten).
            if matches!(outcome, MarkRunningOutcome::Running) {
                wtx.commit()?;
            }
            Ok(outcome)
        })
        .await
        .wrap_err("join mark_running")?
    }

    /// Complete a running attempt. The predicate requires **all four**
    /// (spec §3 v1.1): the child's `current_attempt_id` is this attempt,
    /// the attempt is `Running`, its `generation` equals the fleet's,
    /// and its lease `owner_epoch` matches. On success it writes the
    /// child terminal (`outcome = verdict`), the attempt `Done` with its
    /// `result_snapshot`, settles the budget (`committed += actual`,
    /// `reserved -= reserved_tokens`), and appends `ChildDone`. A
    /// stale/superseded attempt fails the predicate and returns
    /// [`CompleteOutcome::Superseded`] — its result is dropped, nothing
    /// changes.
    #[allow(clippy::too_many_arguments)] // 4-part predicate + settlement inputs are irreducible here
    pub async fn complete_child(
        &self,
        fleet_id: &str,
        child_id: &str,
        attempt_id: &str,
        verdict: AcceptanceVerdict,
        snapshot: ChildResultSnapshot,
        actual_tokens: u64,
        owner_epoch: u64,
        now_ms: u64,
    ) -> Result<CompleteOutcome> {
        ensure_key_safe(&[fleet_id, child_id, attempt_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let attempt_id = attempt_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<CompleteOutcome> {
            let _held = held;
            let wtx = db.begin_write()?;
            let outcome = {
                let mut fleets = wtx.open_table(FLEETS)?;
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let mut attempts = wtx.open_table(ATTEMPTS)?;
                let mut outbox = wtx.open_table(OUTBOX)?;

                let ckey = child_key(&fleet_id, &child_id);
                let akey = attempt_key(&child_id, &attempt_id);

                let Some(cj) = children.get(ckey.as_str())?.map(|g| g.value().to_string()) else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let Some(mut child) = decode_row::<FleetChildRecord>(&cj)? else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let Some(aj) = attempts.get(akey.as_str())?.map(|g| g.value().to_string()) else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let Some(mut attempt) = decode_row::<Attempt>(&aj)? else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let Some(fj) = fleets
                    .get(fleet_id.as_str())?
                    .map(|g| g.value().to_string())
                else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let Some(mut fleet) = decode_row::<FleetRecord>(&fj)? else {
                    return Ok(CompleteOutcome::Superseded);
                };

                // Identity: every decoded row must own the addressed ids
                // (P1-4). A mismatch is treated as a superseded/foreign
                // event, never a cross-fleet mutation.
                if child.fleet_id != fleet_id
                    || child.child_id != child_id
                    || attempt.fleet_id != fleet_id
                    || attempt.child_id != child_id
                    || attempt.attempt_id != attempt_id
                    || fleet.fleet_id != fleet_id
                {
                    return Ok(CompleteOutcome::Superseded);
                }

                // Four-part predicate — attempt-id fences effects,
                // generation fences acceptance, the lease token proves
                // the completer holds the current grant.
                let is_current = child.current_attempt_id.as_deref() == Some(attempt_id.as_str());
                let is_running = attempt.status == AttemptStatus::Running;
                let gen_ok = attempt.generation == fleet.generation;
                let lease_ok = attempt.lease.owner_epoch == owner_epoch;
                if !(is_current && is_running && gen_ok && lease_ok) {
                    return Ok(CompleteOutcome::Superseded);
                }

                // ---- passes: settle terminal state + budget + outbox ----
                let reserved = attempt.reserved_tokens;
                // Settle with checked math: an underflow means a
                // reservation was double-released — an invariant break, so
                // fail loudly rather than mask it (P2-5).
                let new_committed = fleet
                    .budget
                    .tokens_committed
                    .checked_add(actual_tokens)
                    .ok_or_else(|| eyre::eyre!("committed-token overflow completing {child_id}"))?;
                let new_reserved = fleet
                    .budget
                    .tokens_reserved
                    .checked_sub(reserved)
                    .ok_or_else(|| {
                        eyre::eyre!(
                            "reservation underflow completing {child_id}: reserved {reserved} > \
                             fleet.tokens_reserved {}",
                            fleet.budget.tokens_reserved
                        )
                    })?;

                let accepted = matches!(verdict, AcceptanceVerdict::Accepted { .. });
                child.status = if accepted {
                    ChildStatus::Succeeded
                } else {
                    ChildStatus::Failed
                };
                child.outcome = Some(verdict);
                child.tokens_committed = child
                    .tokens_committed
                    .checked_add(actual_tokens)
                    .ok_or_else(|| eyre::eyre!("child committed-token overflow for {child_id}"))?;
                child.updated_at_ms = now_ms;
                children.insert(ckey.as_str(), serde_json::to_string(&child)?.as_str())?;

                attempt.status = AttemptStatus::Done;
                attempt.result_snapshot = Some(snapshot);
                attempt.ended_at_ms = Some(now_ms);
                attempts.insert(akey.as_str(), serde_json::to_string(&attempt)?.as_str())?;

                fleet.budget.tokens_committed = new_committed;
                fleet.budget.tokens_reserved = new_reserved;
                fleet.updated_at_ms = now_ms;
                fleets.insert(fleet_id.as_str(), serde_json::to_string(&fleet)?.as_str())?;

                append_outbox(
                    &mut outbox,
                    outbox_event(
                        FleetEventKind::ChildDone,
                        &fleet_id,
                        Some(&child_id),
                        Some(&attempt_id),
                    ),
                )?;

                CompleteOutcome::Completed
            };
            if matches!(outcome, CompleteOutcome::Completed) {
                wtx.commit()?;
            }
            Ok(outcome)
        })
        .await
        .wrap_err("join complete_child")?
    }

    /// PR B — record a mid-task ESCALATION: settle a running attempt into a
    /// NON-terminal [`ChildStatus::Blocked`] with a [`EscalationRequest`], so
    /// the keeper can widen the grant (`goal_grant` → `Ready`) or deny it
    /// (`goal_deny` → `Failed`).
    ///
    /// It reuses `complete_child`'s EXACT four-part fence (the child's
    /// `current_attempt_id` is this attempt, the attempt is `Running`, its
    /// `generation` equals the fleet's, and its lease `owner_epoch` matches) —
    /// so a stale / superseded / cross-fleet attempt is a no-op
    /// ([`CompleteOutcome::Superseded`]). It differs from `complete_child` only
    /// in the effect: on success it
    ///
    /// - settles the **REAL** tokens the yielded attempt used (`committed +=
    ///   actual_tokens`, `reserved -= reserved_tokens`) — NOT `0`. Budget
    ///   honesty matters here precisely because the child is NON-terminal: a
    ///   FRESH attempt runs after the grant widen, so the budget it is admitted
    ///   against must already reflect the tokens the yielded attempt spent (a
    ///   `0`-commit would let a worker spend the whole budget, escalate, and
    ///   spend it again);
    /// - marks the attempt `Interrupted` (it yielded, it did not complete) and
    ///   **clears** `current_attempt_id`, so the Blocked→Ready transition can
    ///   launch a clean fresh attempt;
    /// - sets the child `Blocked` + `pending_escalation` (its `outcome` stays
    ///   `None` — Blocked is not terminal, so there is no verdict yet);
    /// - appends the SAME [`FleetEventKind::ChildDone`] the completion path
    ///   does, so the EXISTING keeper wake fires with no new machinery.
    #[allow(clippy::too_many_arguments)] // 4-part predicate + escalation payload + settlement inputs are irreducible here
    pub async fn record_escalation(
        &self,
        fleet_id: &str,
        child_id: &str,
        attempt_id: &str,
        request: EscalationRequest,
        actual_tokens: u64,
        owner_epoch: u64,
        now_ms: u64,
    ) -> Result<CompleteOutcome> {
        ensure_key_safe(&[fleet_id, child_id, attempt_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let attempt_id = attempt_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<CompleteOutcome> {
            let _held = held;
            let wtx = db.begin_write()?;
            let outcome = {
                let mut fleets = wtx.open_table(FLEETS)?;
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let mut attempts = wtx.open_table(ATTEMPTS)?;
                let mut outbox = wtx.open_table(OUTBOX)?;

                let ckey = child_key(&fleet_id, &child_id);
                let akey = attempt_key(&child_id, &attempt_id);

                let Some(cj) = children.get(ckey.as_str())?.map(|g| g.value().to_string()) else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let Some(mut child) = decode_row::<FleetChildRecord>(&cj)? else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let Some(aj) = attempts.get(akey.as_str())?.map(|g| g.value().to_string()) else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let Some(mut attempt) = decode_row::<Attempt>(&aj)? else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let Some(fj) = fleets
                    .get(fleet_id.as_str())?
                    .map(|g| g.value().to_string())
                else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let Some(mut fleet) = decode_row::<FleetRecord>(&fj)? else {
                    return Ok(CompleteOutcome::Superseded);
                };

                // Identity: every decoded row must own the addressed ids (P1-4).
                if child.fleet_id != fleet_id
                    || child.child_id != child_id
                    || attempt.fleet_id != fleet_id
                    || attempt.child_id != child_id
                    || attempt.attempt_id != attempt_id
                    || fleet.fleet_id != fleet_id
                {
                    return Ok(CompleteOutcome::Superseded);
                }

                // Same four-part predicate as `complete_child` — a stale / late
                // / superseded attempt cannot escalate.
                let is_current = child.current_attempt_id.as_deref() == Some(attempt_id.as_str());
                let is_running = attempt.status == AttemptStatus::Running;
                let gen_ok = attempt.generation == fleet.generation;
                let lease_ok = attempt.lease.owner_epoch == owner_epoch;
                if !(is_current && is_running && gen_ok && lease_ok) {
                    return Ok(CompleteOutcome::Superseded);
                }

                // ---- passes: settle REAL tokens + Blocked (non-terminal) ----
                let reserved = attempt.reserved_tokens;
                let new_committed = fleet
                    .budget
                    .tokens_committed
                    .checked_add(actual_tokens)
                    .ok_or_else(|| eyre::eyre!("committed-token overflow escalating {child_id}"))?;
                let new_reserved = fleet
                    .budget
                    .tokens_reserved
                    .checked_sub(reserved)
                    .ok_or_else(|| {
                        eyre::eyre!(
                            "reservation underflow escalating {child_id}: reserved {reserved} > \
                             fleet.tokens_reserved {}",
                            fleet.budget.tokens_reserved
                        )
                    })?;

                // Blocked is NON-terminal: no verdict, but the yielded attempt's
                // real token spend IS committed so the fresh attempt's budget is
                // honest. Clear `current_attempt_id` so Blocked→Ready launches
                // clean.
                child.status = ChildStatus::Blocked;
                // PR B (codex round-3, defect 3) — RE-STAMP the row to the current
                // schema. `Blocked` is a v3-only enum variant, so a row carrying it
                // MUST say v3: a legacy v2 child (written before the upgrade, still
                // stamped `schema_version: 2`) that escalates would otherwise persist
                // `{schema_version: 2, status: "Blocked"}` — and a rolled-back v2
                // binary, seeing `2 <= 2`, attempts a full decode and ERRORS on the
                // unknown `Blocked` variant instead of dropping the row as newer.
                // Stamping v3 makes `decode_row` on the old binary drop it (`3 > 2`).
                // This is THE write that introduces `Blocked`, so it is the one that
                // must guarantee the invariant "any persisted `Blocked` row is v3".
                child.schema_version = SCHEMA_VERSION;
                child.pending_escalation = Some(request);
                child.current_attempt_id = None;
                child.tokens_committed = child
                    .tokens_committed
                    .checked_add(actual_tokens)
                    .ok_or_else(|| eyre::eyre!("child committed-token overflow for {child_id}"))?;
                child.updated_at_ms = now_ms;
                children.insert(ckey.as_str(), serde_json::to_string(&child)?.as_str())?;

                // The attempt yielded — Interrupted, not Done (no result).
                attempt.status = AttemptStatus::Interrupted;
                attempt.ended_at_ms = Some(now_ms);
                attempts.insert(akey.as_str(), serde_json::to_string(&attempt)?.as_str())?;

                fleet.budget.tokens_committed = new_committed;
                fleet.budget.tokens_reserved = new_reserved;
                fleet.updated_at_ms = now_ms;
                fleets.insert(fleet_id.as_str(), serde_json::to_string(&fleet)?.as_str())?;

                // The SAME wake the completion path fires — the keeper reads the
                // pending escalation off the fleet view on its next turn.
                append_outbox(
                    &mut outbox,
                    outbox_event(
                        FleetEventKind::ChildDone,
                        &fleet_id,
                        Some(&child_id),
                        Some(&attempt_id),
                    ),
                )?;

                CompleteOutcome::Completed
            };
            if matches!(outcome, CompleteOutcome::Completed) {
                wtx.commit()?;
            }
            Ok(outcome)
        })
        .await
        .wrap_err("join record_escalation")?
    }

    /// Re-plan a fleet (P1-2): revision-fenced full replacement of the
    /// durable plan with clean **interrupt-and-restart** semantics, all in
    /// one write-txn. It CAS-checks `expected_revision`, increments
    /// `fleet.generation`, and then, for every existing child:
    ///
    /// 1. **interrupts any live attempt** (`Leased`/`Running` → `Interrupted`)
    ///    and releases its reservation, clearing `current_attempt_id` — so
    ///    no attempt is left current-and-reserved yet superseded;
    /// 2. **surviving** task → re-stamps the child to the new generation and
    ///    recomputes its state from the NEW deps (`Ready` iff all deps are
    ///    `Succeeded`, else `Planned`); an already-`Succeeded` survivor is
    ///    preserved (completed work is not re-run);
    /// 3. **removed** task → child `Cancelled`;
    /// 4. **new / re-added** task id → a fresh `Ready`/`Planned` child (a
    ///    re-added previously-`Cancelled` task is resurrected).
    ///
    /// Undecodable (higher-schema) child rows are skipped, never
    /// blind-written over. Any attempt stamped with the old generation now
    /// fails both [`FleetKernelStore::complete_child`] and
    /// [`FleetKernelStore::mark_running`]'s generation checks.
    pub async fn replan(
        &self,
        fleet_id: &str,
        expected_revision: u64,
        new_plan: DurablePlan,
        now_ms: u64,
    ) -> Result<PlanMutateOutcome> {
        ensure_key_safe(&[fleet_id])?;
        for t in &new_plan.tasks {
            ensure_key_safe(&[t.task_id.as_str()])?;
            for d in &t.deps {
                ensure_key_safe(&[d.as_str()])?;
            }
        }
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<PlanMutateOutcome> {
            let _held = held;
            let mut new_plan = new_plan;
            let wtx = db.begin_write()?;
            let outcome = {
                let mut fleets = wtx.open_table(FLEETS)?;
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let mut attempts = wtx.open_table(ATTEMPTS)?;
                let mut plans = wtx.open_table(PLANS)?;

                // --- plan + fleet, revision-fenced, identity-checked -----
                let Some(pj) = plans.get(fleet_id.as_str())?.map(|g| g.value().to_string()) else {
                    bail!("replan: no plan for fleet {fleet_id}");
                };
                let Some(plan) = decode_row::<DurablePlan>(&pj)? else {
                    bail!("replan: plan for {fleet_id} is a newer schema; refusing to overwrite");
                };
                if plan.fleet_id != fleet_id {
                    bail!("replan: plan identity mismatch for {fleet_id}");
                }
                if plan.revision != expected_revision {
                    return Ok(PlanMutateOutcome::RevisionMismatch {
                        actual: plan.revision,
                    });
                }

                let Some(fj) = fleets
                    .get(fleet_id.as_str())?
                    .map(|g| g.value().to_string())
                else {
                    bail!("replan: no fleet {fleet_id}");
                };
                let Some(mut fleet) = decode_row::<FleetRecord>(&fj)? else {
                    bail!("replan: fleet {fleet_id} is a newer schema");
                };
                if fleet.fleet_id != fleet_id {
                    bail!("replan: fleet identity mismatch for {fleet_id}");
                }
                let new_gen = fleet
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| eyre::eyre!("generation overflow re-planning {fleet_id}"))?;
                let new_rev = expected_revision
                    .checked_add(1)
                    .ok_or_else(|| eyre::eyre!("revision overflow re-planning {fleet_id}"))?;

                let new_ids: HashSet<String> =
                    new_plan.tasks.iter().map(|t| t.task_id.clone()).collect();

                // --- snapshot this fleet's children, keyed by the *stored*
                // key so we never blind-write over an undecodable
                // (higher-schema / identity-mismatched) row we cannot
                // understand. --------------------------------------------
                let fleet_prefix = format!("{fleet_id}\0");
                let mut existing: Vec<FleetChildRecord> = Vec::new();
                let mut undecodable_ids: HashSet<String> = HashSet::new();
                for item in children.iter()? {
                    let (k, v) = item?;
                    let Some(cid) = k.value().strip_prefix(fleet_prefix.as_str()) else {
                        continue; // belongs to another fleet
                    };
                    match decode_row::<FleetChildRecord>(v.value())? {
                        Some(rec) if rec.fleet_id == fleet_id && rec.child_id == cid => {
                            existing.push(rec)
                        }
                        // Higher-schema or identity-mismatched: opaque —
                        // record the id so the new-task loop never recreates
                        // (overwrites) it.
                        _ => {
                            undecodable_ids.insert(cid.to_string());
                        }
                    }
                }
                let existing_ids: HashSet<String> =
                    existing.iter().map(|c| c.child_id.clone()).collect();
                // Post-replan Succeeded set = children that Succeeded AND
                // survive (removed ones become Cancelled). Drives consistent,
                // order-independent readiness recomputation.
                let succeeded_ids: HashSet<String> = existing
                    .iter()
                    .filter(|c| c.status == ChildStatus::Succeeded && new_ids.contains(&c.child_id))
                    .map(|c| c.child_id.clone())
                    .collect();

                // --- terminal-dep-change guard (round-2 P1), IN-TXN --------
                // A surviving task whose new deps differ from its durable
                // child deps is a `RetargetDeps`-style change. If that child
                // is terminal (`Succeeded`/`Failed`) its spec is frozen —
                // reject with ZERO mutation. Checking this INSIDE the same
                // txn as the revision-CAS closes the race where the caller's
                // out-of-txn pre-check saw the task non-terminal and it then
                // completed before this write. (`Cancelled` is excluded: a
                // removed→re-added task is intentionally resurrected with a
                // fresh spec, so its deps may legitimately change.)
                for task in &new_plan.tasks {
                    if let Some(child) = existing.iter().find(|c| c.child_id == task.task_id) {
                        if child.deps != task.deps
                            && matches!(child.status, ChildStatus::Succeeded | ChildStatus::Failed)
                        {
                            return Ok(PlanMutateOutcome::RejectedTerminalDepChange {
                                task_id: task.task_id.clone(),
                            });
                        }
                    }
                }

                // --- interrupt-and-restart every existing child ----------
                for mut child in existing {
                    let ckey = child_key(&fleet_id, &child.child_id);

                    // 1. Interrupt any LIVE attempt (survivor OR removed)
                    //    and release its reservation — so no attempt is left
                    //    current+reserved yet superseded (P1-2).
                    let mut interrupted = false;
                    if let Some(aid) = child.current_attempt_id.clone() {
                        let akey = attempt_key(&child.child_id, &aid);
                        let atj = attempts.get(akey.as_str())?.map(|g| g.value().to_string());
                        if let Some(atj) = atj {
                            if let Some(mut att) = decode_row::<Attempt>(&atj)? {
                                // Own-id check (P1-4): only interrupt an
                                // attempt whose stored ids match the key.
                                if att.fleet_id == fleet_id
                                    && att.child_id == child.child_id
                                    && att.attempt_id == aid
                                    && matches!(
                                        att.status,
                                        AttemptStatus::Leased | AttemptStatus::Running
                                    )
                                {
                                    let reserved = att.reserved_tokens;
                                    att.status = AttemptStatus::Interrupted;
                                    att.ended_at_ms = Some(now_ms);
                                    attempts.insert(
                                        akey.as_str(),
                                        serde_json::to_string(&att)?.as_str(),
                                    )?;
                                    fleet.budget.tokens_reserved = fleet
                                        .budget
                                        .tokens_reserved
                                        .checked_sub(reserved)
                                        .ok_or_else(|| {
                                            eyre::eyre!(
                                                "reservation underflow interrupting {}",
                                                child.child_id
                                            )
                                        })?;
                                    interrupted = true;
                                }
                            }
                        }
                    }
                    if interrupted {
                        child.current_attempt_id = None;
                    }

                    // 2. Bucket: survivor vs removed.
                    child.generation = new_gen;
                    child.updated_at_ms = now_ms;
                    if new_ids.contains(&child.child_id) {
                        child.deps = new_plan
                            .tasks
                            .iter()
                            .find(|t| t.task_id == child.child_id)
                            .map(|t| t.deps.clone())
                            .unwrap_or_default();
                        if child.status == ChildStatus::Succeeded {
                            // Preserve completed work — keep outcome and the
                            // pointer to its Done attempt.
                        } else {
                            // Recompute a fresh runnable state from the NEW
                            // deps: fixes a survivor kept Ready with newly
                            // unmet deps, resurrects a re-added Cancelled
                            // child, and retries a Failed one. The stale
                            // pointer MUST be cleared for EVERY non-Succeeded
                            // survivor (not only live-interrupted ones) — a
                            // Failed child's Done attempt-id would otherwise
                            // survive and make launch_child reject the retry
                            // as a double-launch (P1).
                            child.current_attempt_id = None;
                            child.outcome = None;
                            // PR B — a replan that resets a `Blocked` survivor must
                            // CLEAR its `pending_escalation`: the child is being
                            // freshly re-readied against the new plan, so a stale
                            // request (whose advisory grant may name capabilities
                            // the operator never approved) must not linger onto the
                            // fresh attempt or a later grant/deny decision.
                            child.pending_escalation = None;
                            child.status = if child.deps.iter().all(|d| succeeded_ids.contains(d)) {
                                ChildStatus::Ready
                            } else {
                                ChildStatus::Planned
                            };
                        }
                    } else {
                        child.status = ChildStatus::Cancelled;
                        child.outcome = Some(AcceptanceVerdict::Terminated {
                            reason: "removed by replan".into(),
                        });
                        child.current_attempt_id = None;
                        child.pending_escalation = None;
                    }
                    children.insert(ckey.as_str(), serde_json::to_string(&child)?.as_str())?;
                }

                // --- add children for brand-new task ids (never over an
                //     existing or undecodable row) --------------------------
                for task in &new_plan.tasks {
                    if existing_ids.contains(&task.task_id)
                        || undecodable_ids.contains(&task.task_id)
                    {
                        continue;
                    }
                    let status = if task.deps.iter().all(|d| succeeded_ids.contains(d)) {
                        ChildStatus::Ready
                    } else {
                        ChildStatus::Planned
                    };
                    let rec = FleetChildRecord {
                        schema_version: SCHEMA_VERSION,
                        fleet_id: fleet_id.clone(),
                        child_id: task.task_id.clone(),
                        worker_kind: WorkerKind::StatelessTask,
                        status,
                        current_attempt_id: None,
                        attempts_used: 0,
                        outcome: None,
                        tokens_committed: 0,
                        deps: task.deps.clone(),
                        pending_escalation: None,
                        generation: new_gen,
                        updated_at_ms: now_ms,
                    };
                    let ckey = child_key(&fleet_id, &task.task_id);
                    children.insert(ckey.as_str(), serde_json::to_string(&rec)?.as_str())?;
                }

                new_plan.schema_version = SCHEMA_VERSION;
                new_plan.fleet_id = fleet_id.clone();
                new_plan.revision = new_rev;
                plans.insert(
                    fleet_id.as_str(),
                    serde_json::to_string(&new_plan)?.as_str(),
                )?;

                fleet.generation = new_gen;
                fleet.updated_at_ms = now_ms;
                fleets.insert(fleet_id.as_str(), serde_json::to_string(&fleet)?.as_str())?;

                PlanMutateOutcome::Mutated { revision: new_rev }
            };
            if matches!(outcome, PlanMutateOutcome::Mutated { .. }) {
                wtx.commit()?;
            }
            Ok(outcome)
        })
        .await
        .wrap_err("join replan")?
    }

    /// Spec-only plan edit: revision-fenced update of one task's `title` +
    /// `detail`, in one write-txn, that **does not** bump `fleet.generation`
    /// or touch any child (P2-c). Title/detail have no execution impact, so
    /// unlike [`FleetKernelStore::replan`] this must **not** interrupt live
    /// attempts or reset children. CAS on `expected_revision` (bumps the
    /// plan's `revision` only); a stale revision yields
    /// [`PlanMutateOutcome::RevisionMismatch`]. Errors if the task is not in
    /// the plan.
    pub async fn retitle_task(
        &self,
        fleet_id: &str,
        expected_revision: u64,
        task_id: &str,
        title: &str,
        detail: &str,
        now_ms: u64,
    ) -> Result<PlanMutateOutcome> {
        ensure_key_safe(&[fleet_id, task_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let task_id = task_id.to_string();
        let title = title.to_string();
        let detail = detail.to_string();
        let _ = now_ms; // spec-only: no timestamped row is written here
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<PlanMutateOutcome> {
            let _held = held;
            let wtx = db.begin_write()?;
            let outcome = {
                let mut plans = wtx.open_table(PLANS)?;
                let Some(pj) = plans.get(fleet_id.as_str())?.map(|g| g.value().to_string()) else {
                    bail!("retitle_task: no plan for fleet {fleet_id}");
                };
                let Some(mut plan) = decode_row::<DurablePlan>(&pj)? else {
                    bail!("retitle_task: plan for {fleet_id} is a newer schema");
                };
                if plan.fleet_id != fleet_id {
                    bail!("retitle_task: plan identity mismatch for {fleet_id}");
                }
                if plan.revision != expected_revision {
                    return Ok(PlanMutateOutcome::RevisionMismatch {
                        actual: plan.revision,
                    });
                }
                let Some(task) = plan.tasks.iter_mut().find(|t| t.task_id == task_id) else {
                    bail!("retitle_task: task {task_id} not in fleet {fleet_id}'s plan");
                };
                task.title = title;
                task.detail = detail;
                let new_rev = expected_revision
                    .checked_add(1)
                    .ok_or_else(|| eyre::eyre!("revision overflow retitling {fleet_id}"))?;
                plan.revision = new_rev;
                plan.schema_version = SCHEMA_VERSION;
                plans.insert(fleet_id.as_str(), serde_json::to_string(&plan)?.as_str())?;
                PlanMutateOutcome::Mutated { revision: new_rev }
            };
            if matches!(outcome, PlanMutateOutcome::Mutated { .. }) {
                wtx.commit()?;
            }
            Ok(outcome)
        })
        .await
        .wrap_err("join retitle_task")?
    }

    /// PR B — apply an operator grant WIDEN to one task and resume its blocked
    /// child, in one revision-fenced write-txn. This is the `goal_grant` store
    /// op: the keeper approved a worker's mid-task escalation, so the task's
    /// [`PlanTask::grant`] is replaced with the keeper-chosen grant and the
    /// yielded child is transitioned `Blocked → Ready` (its
    /// `pending_escalation` cleared) — a fresh attempt then rebuilds its
    /// registry + sandbox from the NEW, wider grant.
    ///
    /// It is DELIBERATELY targeted, NOT a [`FleetKernelStore::replan`]: it bumps
    /// only the plan `revision` (never `fleet.generation`) and touches only THIS
    /// task's row — no blast radius across other children, no re-ready of every
    /// non-`Succeeded` child. A generation bump is unnecessary because the
    /// yielded attempt is ALREADY settled (`record_escalation` interrupted it
    /// and cleared `current_attempt_id` before this edit): there is no live
    /// attempt to fence off.
    ///
    /// CAS on `expected_revision` (stale → [`PlanMutateOutcome::RevisionMismatch`],
    /// nothing changes). Errors if the task is absent from the plan. The child
    /// transition is applied only when the child is `Blocked` (the escalation
    /// case); a non-`Blocked` child keeps its status (the grant still updates),
    /// so a grant edit is inert on execution state outside the escalation flow.
    pub async fn set_task_grant(
        &self,
        fleet_id: &str,
        expected_revision: u64,
        task_id: &str,
        grant: WorkerGrant,
        now_ms: u64,
    ) -> Result<PlanMutateOutcome> {
        ensure_key_safe(&[fleet_id, task_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let task_id = task_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<PlanMutateOutcome> {
            let _held = held;
            let wtx = db.begin_write()?;
            let outcome = {
                let mut plans = wtx.open_table(PLANS)?;
                let mut children = wtx.open_table(FLEET_CHILDREN)?;

                let Some(pj) = plans.get(fleet_id.as_str())?.map(|g| g.value().to_string()) else {
                    bail!("set_task_grant: no plan for fleet {fleet_id}");
                };
                let Some(mut plan) = decode_row::<DurablePlan>(&pj)? else {
                    bail!("set_task_grant: plan for {fleet_id} is a newer schema");
                };
                if plan.fleet_id != fleet_id {
                    bail!("set_task_grant: plan identity mismatch for {fleet_id}");
                }
                if plan.revision != expected_revision {
                    return Ok(PlanMutateOutcome::RevisionMismatch {
                        actual: plan.revision,
                    });
                }
                let Some(task_idx) = plan.tasks.iter().position(|t| t.task_id == task_id) else {
                    bail!("set_task_grant: task {task_id} not in fleet {fleet_id}'s plan");
                };

                // PR B (codex round-2) — CAS on the child being `Blocked` INSIDE
                // the txn, BEFORE mutating the plan. The grant+resume applies ONLY
                // to a child still awaiting the operator: if a concurrent
                // `deny_escalation` moved it `Blocked → Failed` (and bumped the
                // revision), or a prior grant already resumed it, this child is no
                // longer `Blocked` and the (possibly stale/denied) grant is
                // REFUSED with ZERO mutation. Grant and deny are mutually
                // exclusive. Materialize the read into an owned String first so no
                // access guard borrows `children` across the later `insert`.
                let ckey = child_key(&fleet_id, &task_id);
                let cj = children.get(ckey.as_str())?.map(|g| g.value().to_string());
                let refused = PlanMutateOutcome::RejectedNotBlocked {
                    task_id: task_id.clone(),
                };
                let Some(cj) = cj else {
                    return Ok(refused);
                };
                let Some(mut child) = decode_row::<FleetChildRecord>(&cj)? else {
                    return Ok(refused);
                };
                if child.fleet_id != fleet_id
                    || child.child_id != task_id
                    || child.status != ChildStatus::Blocked
                {
                    return Ok(refused);
                }

                // Blocked confirmed — apply the grant to the plan, bump the
                // revision, and resume the child (Blocked → Ready, request
                // cleared). A Ready child MUST have no live attempt or
                // launch_child rejects the relaunch as a double-launch.
                let new_rev = expected_revision
                    .checked_add(1)
                    .ok_or_else(|| eyre::eyre!("revision overflow granting {fleet_id}"))?;
                plan.tasks[task_idx].grant = grant;
                plan.revision = new_rev;
                plan.schema_version = SCHEMA_VERSION;
                plans.insert(fleet_id.as_str(), serde_json::to_string(&plan)?.as_str())?;

                child.status = ChildStatus::Ready;
                child.pending_escalation = None;
                child.current_attempt_id = None;
                child.updated_at_ms = now_ms;
                children.insert(ckey.as_str(), serde_json::to_string(&child)?.as_str())?;

                PlanMutateOutcome::Mutated { revision: new_rev }
            };
            if matches!(outcome, PlanMutateOutcome::Mutated { .. }) {
                wtx.commit()?;
            }
            Ok(outcome)
        })
        .await
        .wrap_err("join set_task_grant")?
    }

    /// PR B — deny a worker's mid-task escalation, moving its blocked child
    /// `Blocked → Failed` (TERMINAL), in one write-txn. This is the `goal_deny`
    /// store op: the keeper refused the requested grant widen, so the task
    /// cannot proceed. Terminality is load-bearing — a `Blocked` child is
    /// non-terminal and holds `is_complete` open forever, so a denial that left
    /// it `Blocked` would WEDGE the fleet. The child is stamped a
    /// [`AcceptanceVerdict::Rejected`] with the keeper's reason and its
    /// `pending_escalation` cleared.
    ///
    /// A no-op ([`CompleteOutcome::Superseded`]) if the child is not `Blocked`
    /// (nothing to deny) — so a double-deny, or a deny racing a grant, cannot
    /// clobber a child that already resumed.
    ///
    /// PR B (codex round-4, defect 2) — it also computes, IN THE SAME write-txn,
    /// whether the fleet is now un-completable (a `Failed` task strands it) and
    /// returns it on the [`DenyEscalationOutcome`]. The keeper drives the goal
    /// terminal DIRECTLY from that returned value, so the goal-terminal resolution
    /// can never be skipped by a separate fallible read after the durable deny.
    pub async fn deny_escalation(
        &self,
        fleet_id: &str,
        child_id: &str,
        reason: &str,
        now_ms: u64,
    ) -> Result<DenyEscalationOutcome> {
        ensure_key_safe(&[fleet_id, child_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let reason = reason.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<DenyEscalationOutcome> {
            let _held = held;
            // A no-op deny changed nothing, so the fleet's completability is
            // untouched (`false`).
            let superseded = DenyEscalationOutcome {
                settled: CompleteOutcome::Superseded,
                fleet_un_completable: false,
            };
            let wtx = db.begin_write()?;
            let outcome = {
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let mut plans = wtx.open_table(PLANS)?;
                let mut outbox = wtx.open_table(OUTBOX)?;
                let ckey = child_key(&fleet_id, &child_id);
                let Some(cj) = children.get(ckey.as_str())?.map(|g| g.value().to_string()) else {
                    return Ok(superseded);
                };
                let Some(mut child) = decode_row::<FleetChildRecord>(&cj)? else {
                    return Ok(superseded);
                };
                // Only a Blocked child can be denied (identity-checked, P1-4).
                if child.fleet_id != fleet_id
                    || child.child_id != child_id
                    || child.status != ChildStatus::Blocked
                {
                    return Ok(superseded);
                }
                child.status = ChildStatus::Failed;
                child.outcome = Some(AcceptanceVerdict::Rejected {
                    reason: format!("escalation denied: {reason}"),
                });
                child.pending_escalation = None;
                child.current_attempt_id = None;
                child.updated_at_ms = now_ms;
                children.insert(ckey.as_str(), serde_json::to_string(&child)?.as_str())?;

                // PR B (codex round-2) — BUMP the plan revision so a concurrent
                // `set_task_grant` that read this child `Blocked` at revision N
                // fails its revision CAS: the grant and the deny cannot both
                // commit, so a denied capability can never be applied. (The
                // grant's in-txn `Blocked` CAS is the belt; this bump is the
                // suspenders — whichever commits first, the other is refused.)
                // A missing plan row is a corruption we do not silently create.
                // Materialize the read into an owned String FIRST so no access
                // guard borrows `plans` across the later `insert`.
                //
                // PR B (codex round-4, defect 2) — the SAME plan drives the
                // completability computation below (its task list is the ground
                // truth for "can this fleet still reach all-Succeeded?").
                let pj = plans.get(fleet_id.as_str())?.map(|g| g.value().to_string());
                let mut fleet_un_completable = false;
                if let Some(pj) = pj {
                    if let Some(mut plan) = decode_row::<DurablePlan>(&pj)? {
                        if plan.fleet_id == fleet_id {
                            plan.revision = plan.revision.checked_add(1).ok_or_else(|| {
                                eyre::eyre!("revision overflow denying escalation for {fleet_id}")
                            })?;
                            plan.schema_version = SCHEMA_VERSION;
                            plans.insert(
                                fleet_id.as_str(),
                                serde_json::to_string(&plan)?.as_str(),
                            )?;

                            // Completability, computed on the durable POST-deny
                            // child states — mirrors the goal snapshot's rule
                            // (`!all-Succeeded && any-Failed`). The just-denied
                            // child is `Failed` (already written), so this is
                            // always `true` here; computing it over the real plan
                            // (rather than assuming) keeps it faithful if the plan
                            // shape changes. The denied child is read from the
                            // local `child` (avoids a redundant re-decode); the
                            // rest are read back (owned String first, so no access
                            // guard borrows `children` across iterations).
                            let mut all_succeeded = true;
                            let mut any_failed = false;
                            for task in &plan.tasks {
                                let status = if task.task_id == child_id {
                                    child.status
                                } else {
                                    let tkey = child_key(&fleet_id, &task.task_id);
                                    match children
                                        .get(tkey.as_str())?
                                        .map(|g| g.value().to_string())
                                    {
                                        Some(j) => decode_row::<FleetChildRecord>(&j)?
                                            .map(|c| c.status)
                                            .unwrap_or(ChildStatus::Planned),
                                        None => ChildStatus::Planned,
                                    }
                                };
                                if status != ChildStatus::Succeeded {
                                    all_succeeded = false;
                                }
                                if status == ChildStatus::Failed {
                                    any_failed = true;
                                }
                            }
                            fleet_un_completable = !all_succeeded && any_failed;
                        }
                    }
                }

                // PR B (codex round-2) — EMIT the same ChildDone wake the
                // completion/escalation paths do, so the keeper is woken to
                // re-evaluate a denied task (its goal reaches a terminal state
                // instead of staying perpetually active on the now-Failed child).
                append_outbox(
                    &mut outbox,
                    outbox_event(FleetEventKind::ChildDone, &fleet_id, Some(&child_id), None),
                )?;
                DenyEscalationOutcome {
                    settled: CompleteOutcome::Completed,
                    fleet_un_completable,
                }
            };
            if matches!(outcome.settled, CompleteOutcome::Completed) {
                wtx.commit()?;
            }
            Ok(outcome)
        })
        .await
        .wrap_err("join deny_escalation")?
    }

    /// Boot recovery: scan children, and for each `Launching`/`Running`
    /// child whose live attempt's lease is stale (foreign `owner_epoch`
    /// **or** expired at `now_ms`), atomically mark the attempt
    /// `Interrupted`, **release its reservation** from the fleet budget
    /// (P1-1), clear `current_attempt_id`, and return the child to
    /// `Ready`. Children of a `Cancelled` fleet are left terminal, never
    /// resurrected (P2-7). The old attempt is never resurrected; all
    /// reclamations commit together.
    pub async fn reconcile(&self, now_ms: u64, owner_epoch: u64) -> Result<ReconcileReport> {
        let db = self.db.clone();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<ReconcileReport> {
            let _held = held;
            let wtx = db.begin_write()?;
            let mut report = ReconcileReport::default();
            {
                let mut fleets = wtx.open_table(FLEETS)?;
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let mut attempts = wtx.open_table(ATTEMPTS)?;

                // Collect owned candidate rows first so the iterator's
                // borrow is released before we mutate the same table.
                let candidates: Vec<FleetChildRecord> = {
                    let mut out = Vec::new();
                    for item in children.iter()? {
                        let (k, v) = item?;
                        if let Some(rec) = decode_row::<FleetChildRecord>(v.value())? {
                            // Defense in depth (P1-4): the decoded record
                            // must own the key it is stored under.
                            if child_key(&rec.fleet_id, &rec.child_id) == k.value() {
                                out.push(rec);
                            }
                        }
                    }
                    out
                };

                for mut child in candidates {
                    if !matches!(child.status, ChildStatus::Launching | ChildStatus::Running) {
                        continue;
                    }
                    let Some(aid) = child.current_attempt_id.clone() else {
                        continue;
                    };

                    // Load the owning fleet: needed to skip Cancelled
                    // fleets (P2-7) and to release the reservation (P1-1).
                    let Some(fj) = fleets
                        .get(child.fleet_id.as_str())?
                        .map(|g| g.value().to_string())
                    else {
                        continue;
                    };
                    let Some(mut fleet) = decode_row::<FleetRecord>(&fj)? else {
                        continue;
                    };
                    // Own-id check (P1-4): the decoded fleet must be the one
                    // we addressed before we mutate its budget.
                    if fleet.fleet_id != child.fleet_id {
                        continue;
                    }
                    if fleet.status == FleetStatus::Cancelled {
                        continue;
                    }

                    let akey = attempt_key(&child.child_id, &aid);
                    let Some(aj) = attempts.get(akey.as_str())?.map(|g| g.value().to_string())
                    else {
                        continue;
                    };
                    let Some(mut attempt) = decode_row::<Attempt>(&aj)? else {
                        continue;
                    };
                    // Defense in depth (P1-4): the attempt must own the ids
                    // it was addressed by.
                    if attempt.child_id != child.child_id
                        || attempt.attempt_id != aid
                        || attempt.fleet_id != child.fleet_id
                    {
                        continue;
                    }
                    if !matches!(
                        attempt.status,
                        AttemptStatus::Leased | AttemptStatus::Running
                    ) {
                        continue;
                    }
                    let stale = attempt.lease.owner_epoch != owner_epoch
                        || attempt.lease.expires_at_ms < now_ms;
                    if !stale {
                        continue;
                    }

                    let reserved = attempt.reserved_tokens;
                    attempt.status = AttemptStatus::Interrupted;
                    attempt.ended_at_ms = Some(now_ms);
                    attempts.insert(akey.as_str(), serde_json::to_string(&attempt)?.as_str())?;

                    // Release the stranded reservation (P1-1). An
                    // underflow is an accounting invariant break.
                    fleet.budget.tokens_reserved = fleet
                        .budget
                        .tokens_reserved
                        .checked_sub(reserved)
                        .ok_or_else(|| {
                            eyre::eyre!(
                                "reconcile reservation underflow for {}: reserved {reserved} > {}",
                                child.child_id,
                                fleet.budget.tokens_reserved
                            )
                        })?;
                    fleet.updated_at_ms = now_ms;
                    fleets.insert(
                        child.fleet_id.as_str(),
                        serde_json::to_string(&fleet)?.as_str(),
                    )?;

                    let ckey = child_key(&child.fleet_id, &child.child_id);
                    child.status = ChildStatus::Ready;
                    child.current_attempt_id = None;
                    child.updated_at_ms = now_ms;
                    children.insert(ckey.as_str(), serde_json::to_string(&child)?.as_str())?;

                    report.interrupted.push(InterruptedAttempt {
                        fleet_id: child.fleet_id.clone(),
                        child_id: child.child_id.clone(),
                        attempt_id: aid,
                    });
                }
            }
            wtx.commit()?;
            Ok(report)
        })
        .await
        .wrap_err("join reconcile")?
    }

    // ---- gated reads ------------------------------------------------------

    /// Read a fleet + its plan + all its children as one internally
    /// consistent [`FleetSnapshot`], under a **single** `begin_read`
    /// transaction (one `io_gate` acquisition) — so the plan, fleet, and
    /// children are all from the same instant and a concurrent `replan`
    /// cannot tear the read into an old-plan + new-children mix (P1-c).
    /// Returns `None` if the fleet does not exist (or is a higher schema).
    pub async fn load_snapshot(&self, fleet_id: &str) -> Result<Option<FleetSnapshot>> {
        ensure_key_safe(&[fleet_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<Option<FleetSnapshot>> {
            let _held = held;
            let rtx = db.begin_read()?;

            // Fleet — the anchor. Absent → no snapshot.
            let fj = {
                let fleets = rtx.open_table(FLEETS)?;
                fleets
                    .get(fleet_id.as_str())?
                    .map(|g| g.value().to_string())
            };
            let Some(fj) = fj else {
                return Ok(None);
            };
            let Some(fleet) = decode_row::<FleetRecord>(&fj)?.filter(|r| r.fleet_id == fleet_id)
            else {
                return Ok(None);
            };

            // Plan (optional) — same read txn.
            let pj = {
                let plans = rtx.open_table(PLANS)?;
                plans.get(fleet_id.as_str())?.map(|g| g.value().to_string())
            };
            let plan = match pj {
                Some(pj) => decode_row::<DurablePlan>(&pj)?.filter(|p| p.fleet_id == fleet_id),
                None => None,
            };

            // Children — same read txn.
            let mut children = Vec::new();
            {
                let table = rtx.open_table(FLEET_CHILDREN)?;
                for item in table.iter()? {
                    let (_, v) = item?;
                    if let Some(rec) = decode_row::<FleetChildRecord>(v.value())? {
                        if rec.fleet_id == fleet_id {
                            children.push(rec);
                        }
                    }
                }
            }
            children.sort_by(|a, b| a.child_id.cmp(&b.child_id));

            Ok(Some(FleetSnapshot {
                fleet,
                plan,
                children,
            }))
        })
        .await
        .wrap_err("join load_snapshot")?
    }

    /// Fetch a fleet (dropping a higher-schema row as `Ok(None)`).
    pub async fn get_fleet(&self, fleet_id: &str) -> Result<Option<FleetRecord>> {
        ensure_key_safe(&[fleet_id])?;
        let db = self.db.clone();
        let key = fleet_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<Option<FleetRecord>> {
            let _held = held;
            let rtx = db.begin_read()?;
            let table = rtx.open_table(FLEETS)?;
            let Some(v) = table.get(key.as_str())? else {
                return Ok(None);
            };
            Ok(decode_row::<FleetRecord>(v.value())?.filter(|r| r.fleet_id == key))
        })
        .await
        .wrap_err("join get_fleet")?
    }

    /// Fetch a child.
    pub async fn get_child(
        &self,
        fleet_id: &str,
        child_id: &str,
    ) -> Result<Option<FleetChildRecord>> {
        ensure_key_safe(&[fleet_id, child_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let key = child_key(&fleet_id, &child_id);
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<Option<FleetChildRecord>> {
            let _held = held;
            let rtx = db.begin_read()?;
            let table = rtx.open_table(FLEET_CHILDREN)?;
            let Some(v) = table.get(key.as_str())? else {
                return Ok(None);
            };
            Ok(decode_row::<FleetChildRecord>(v.value())?
                .filter(|r| r.fleet_id == fleet_id && r.child_id == child_id))
        })
        .await
        .wrap_err("join get_child")?
    }

    /// List a fleet's children, sorted by `child_id`.
    pub async fn list_children(&self, fleet_id: &str) -> Result<Vec<FleetChildRecord>> {
        ensure_key_safe(&[fleet_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<Vec<FleetChildRecord>> {
            let _held = held;
            let rtx = db.begin_read()?;
            let table = rtx.open_table(FLEET_CHILDREN)?;
            let mut out = Vec::new();
            for item in table.iter()? {
                let (_, v) = item?;
                if let Some(rec) = decode_row::<FleetChildRecord>(v.value())? {
                    if rec.fleet_id == fleet_id {
                        out.push(rec);
                    }
                }
            }
            out.sort_by(|a, b| a.child_id.cmp(&b.child_id));
            Ok(out)
        })
        .await
        .wrap_err("join list_children")?
    }

    /// Fetch the durable plan.
    pub async fn get_plan(&self, fleet_id: &str) -> Result<Option<DurablePlan>> {
        ensure_key_safe(&[fleet_id])?;
        let db = self.db.clone();
        let key = fleet_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<Option<DurablePlan>> {
            let _held = held;
            let rtx = db.begin_read()?;
            let table = rtx.open_table(PLANS)?;
            let Some(v) = table.get(key.as_str())? else {
                return Ok(None);
            };
            Ok(decode_row::<DurablePlan>(v.value())?.filter(|p| p.fleet_id == key))
        })
        .await
        .wrap_err("join get_plan")?
    }

    /// Fetch a specific attempt.
    pub async fn get_attempt(&self, child_id: &str, attempt_id: &str) -> Result<Option<Attempt>> {
        ensure_key_safe(&[child_id, attempt_id])?;
        let db = self.db.clone();
        let child_id = child_id.to_string();
        let attempt_id = attempt_id.to_string();
        let key = attempt_key(&child_id, &attempt_id);
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<Option<Attempt>> {
            let _held = held;
            let rtx = db.begin_read()?;
            let table = rtx.open_table(ATTEMPTS)?;
            let Some(v) = table.get(key.as_str())? else {
                return Ok(None);
            };
            Ok(decode_row::<Attempt>(v.value())?
                .filter(|a| a.child_id == child_id && a.attempt_id == attempt_id))
        })
        .await
        .wrap_err("join get_attempt")?
    }

    // ---- outbox -----------------------------------------------------------

    /// Append an outbox event, assigning the next monotonic sequence.
    /// Returns the assigned sequence.
    pub async fn append_event(&self, event: OutboxEvent) -> Result<u64> {
        ensure_key_safe(&[event.fleet_id.as_str(), event.event_id.as_str()])?;
        if let Some(c) = &event.child_id {
            ensure_key_safe(&[c.as_str()])?;
        }
        if let Some(a) = &event.attempt_id {
            ensure_key_safe(&[a.as_str()])?;
        }
        let db = self.db.clone();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<u64> {
            let _held = held;
            let wtx = db.begin_write()?;
            let seq = {
                let mut outbox = wtx.open_table(OUTBOX)?;
                append_outbox(&mut outbox, event)?
            };
            wtx.commit()?;
            Ok(seq)
        })
        .await
        .wrap_err("join append_event")?
    }

    /// Claim the lowest-sequence unacked event whose claim is free or
    /// expired, stamping `claimed_by` + a fresh unique `claim_token` +
    /// `claim_expires_at`. Returns the claimed event (carrying its
    /// `claim_token`, which the consumer must present to
    /// [`FleetKernelStore::ack`]). `None` when nothing is claimable.
    pub async fn claim_next(
        &self,
        consumer: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Option<OutboxEvent>> {
        let db = self.db.clone();
        let consumer = consumer.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<Option<OutboxEvent>> {
            let _held = held;
            let wtx = db.begin_write()?;
            let claimed = {
                let mut outbox = wtx.open_table(OUTBOX)?;
                let mut found: Option<(String, OutboxEvent)> = None;
                for item in outbox.iter()? {
                    let (k, v) = item?;
                    let Some(ev) = decode_row::<OutboxEvent>(v.value())? else {
                        continue;
                    };
                    let claim_free = ev.claimed_by.is_none()
                        || ev.claim_expires_at.map(|e| e < now_ms).unwrap_or(true);
                    if !ev.acked && claim_free {
                        found = Some((k.value().to_string(), ev));
                        break;
                    }
                }
                match found {
                    Some((key, mut ev)) => {
                        let expires = now_ms
                            .checked_add(ttl_ms)
                            .ok_or_else(|| eyre::eyre!("claim-expiry overflow"))?;
                        ev.claimed_by = Some(consumer);
                        ev.claim_token = Some(Uuid::new_v4().to_string());
                        ev.claim_expires_at = Some(expires);
                        outbox.insert(key.as_str(), serde_json::to_string(&ev)?.as_str())?;
                        Some(ev)
                    }
                    None => None,
                }
            };
            if claimed.is_some() {
                wtx.commit()?;
            }
            Ok(claimed)
        })
        .await
        .wrap_err("join claim_next")?
    }

    /// Acknowledge (consume) an outbox event, **claim-fenced** (P1-3):
    /// the presented `(consumer, claim_token)` must match the row's
    /// current claim, else it returns [`AckOutcome::StaleClaim`] and
    /// changes nothing — so a stale consumer whose lease already expired
    /// and was reclaimed cannot ack an event out from under the new
    /// owner. Errors if the sequence does not exist.
    pub async fn ack(
        &self,
        sequence: u64,
        consumer: &str,
        claim_token: &str,
    ) -> Result<AckOutcome> {
        let db = self.db.clone();
        let consumer = consumer.to_string();
        let claim_token = claim_token.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<AckOutcome> {
            let _held = held;
            let wtx = db.begin_write()?;
            let outcome = {
                let mut outbox = wtx.open_table(OUTBOX)?;
                let key = outbox_key(sequence);
                let Some(vj) = outbox.get(key.as_str())?.map(|g| g.value().to_string()) else {
                    bail!("ack: outbox sequence {sequence} not found");
                };
                let Some(mut ev) = decode_row::<OutboxEvent>(&vj)? else {
                    bail!("outbox event {sequence} is a newer schema");
                };
                let claim_matches = ev.claimed_by.as_deref() == Some(consumer.as_str())
                    && ev.claim_token.as_deref() == Some(claim_token.as_str());
                if claim_matches {
                    ev.acked = true;
                    outbox.insert(key.as_str(), serde_json::to_string(&ev)?.as_str())?;
                    AckOutcome::Acked
                } else {
                    AckOutcome::StaleClaim
                }
            };
            if matches!(outcome, AckOutcome::Acked) {
                wtx.commit()?;
            }
            Ok(outcome)
        })
        .await
        .wrap_err("join ack")?
    }

    // ---- decision log -----------------------------------------------------

    /// Append an entry to a fleet's append-only decision log, assigning
    /// the next per-fleet sequence. Returns the assigned sequence.
    pub async fn append_decision(
        &self,
        fleet_id: &str,
        actor: &str,
        kind: DecisionKind,
        note: &str,
        now_ms: u64,
    ) -> Result<u64> {
        ensure_key_safe(&[fleet_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let actor = actor.to_string();
        let note = note.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<u64> {
            let _held = held;
            let wtx = db.begin_write()?;
            let seq = {
                let mut log = wtx.open_table(DECISION_LOG)?;
                let mut max = 0u64;
                for item in log.iter()? {
                    let (k, _) = item?;
                    if let Some(s) = decision_seq(k.value(), &fleet_id) {
                        max = max.max(s);
                    }
                }
                let next = max
                    .checked_add(1)
                    .ok_or_else(|| eyre::eyre!("decision sequence overflow for {fleet_id}"))?;
                let entry = DecisionEntry {
                    schema_version: SCHEMA_VERSION,
                    seq: next,
                    at_ms: now_ms,
                    actor,
                    kind,
                    note,
                };
                let key = decision_key(&fleet_id, next);
                log.insert(key.as_str(), serde_json::to_string(&entry)?.as_str())?;
                next
            };
            wtx.commit()?;
            Ok(seq)
        })
        .await
        .wrap_err("join append_decision")?
    }

    /// List a fleet's decision log in sequence order.
    pub async fn list_decisions(&self, fleet_id: &str) -> Result<Vec<DecisionEntry>> {
        ensure_key_safe(&[fleet_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<Vec<DecisionEntry>> {
            let _held = held;
            let rtx = db.begin_read()?;
            let table = rtx.open_table(DECISION_LOG)?;
            let mut out = Vec::new();
            for item in table.iter()? {
                let (k, v) = item?;
                if decision_seq(k.value(), &fleet_id).is_some() {
                    if let Some(entry) = decode_row::<DecisionEntry>(v.value())? {
                        out.push(entry);
                    }
                }
            }
            out.sort_by_key(|e| e.seq);
            Ok(out)
        })
        .await
        .wrap_err("join list_decisions")?
    }
}

// ---------------------------------------------------------------------------
// Key helpers + validation + in-txn outbox append
// ---------------------------------------------------------------------------

/// A persisted key component is safe iff it is non-empty and carries no
/// control/NUL characters — so `\0`-delimited composite keys are
/// unambiguous and cannot be cross-addressed (P1-4). Mirrors the peer
/// code's `peer_slug_is_safe`.
fn key_component_is_safe(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_control())
}

/// Reject any unsafe key component up front with a typed error.
fn ensure_key_safe(components: &[&str]) -> Result<()> {
    for c in components {
        if !key_component_is_safe(c) {
            bail!(
                "invalid key component {c:?}: must be non-empty and free of control/NUL characters"
            );
        }
    }
    Ok(())
}

fn child_key(fleet_id: &str, child_id: &str) -> String {
    format!("{fleet_id}\0{child_id}")
}

fn attempt_key(child_id: &str, attempt_id: &str) -> String {
    format!("{child_id}\0{attempt_id}")
}

fn decision_key(fleet_id: &str, seq: u64) -> String {
    format!("{fleet_id}\0{seq:020}")
}

fn outbox_key(seq: u64) -> String {
    format!("{seq:020}")
}

/// Parse the per-fleet sequence out of a `decision_log` key, if it
/// belongs to `fleet_id`.
fn decision_seq(key: &str, fleet_id: &str) -> Option<u64> {
    let prefix = format!("{fleet_id}\0");
    key.strip_prefix(&prefix)?.parse::<u64>().ok()
}

/// Build a fresh, unclaimed outbox event (sequence assigned on append).
fn outbox_event(
    kind: FleetEventKind,
    fleet_id: &str,
    child_id: Option<&str>,
    attempt_id: Option<&str>,
) -> OutboxEvent {
    OutboxEvent {
        schema_version: SCHEMA_VERSION,
        sequence: 0,
        event_id: Uuid::new_v4().to_string(),
        fleet_id: fleet_id.to_string(),
        child_id: child_id.map(str::to_string),
        attempt_id: attempt_id.map(str::to_string),
        kind,
        payload: serde_json::Value::Null,
        claimed_by: None,
        claim_token: None,
        claim_expires_at: None,
        acked: false,
    }
}

/// Append `event` to an already-open outbox table, assigning the next
/// monotonic sequence (max existing key + 1) with **checked** math — a
/// corrupt last key or a sequence overflow is an error, never a silent
/// overwrite of the last row (P2-5). Kept as a free function so the
/// launch/complete CAS ops append **inside their own write-txn** — never
/// by re-entering the gated `append_event` (which would deadlock on the
/// `io_gate`).
fn append_outbox(outbox: &mut redb::Table<'_, &str, &str>, mut event: OutboxEvent) -> Result<u64> {
    let next = match outbox.last()? {
        Some((k, _)) => {
            let last: u64 = k
                .value()
                .parse()
                .map_err(|e| eyre::eyre!("corrupt outbox key {:?}: {e}", k.value()))?;
            last.checked_add(1)
                .ok_or_else(|| eyre::eyre!("outbox sequence overflow"))?
        }
        None => 1,
    };
    event.sequence = next;
    event.schema_version = SCHEMA_VERSION;
    let key = outbox_key(next);
    outbox.insert(key.as_str(), serde_json::to_string(&event)?.as_str())?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const EPOCH: u64 = 100;
    const TTL: u64 = 1_000;

    async fn fresh() -> (TempDir, FleetKernelStore) {
        let dir = TempDir::new().unwrap();
        let store = FleetKernelStore::open(dir.path()).await.unwrap();
        (dir, store)
    }

    fn controller() -> SessionKey {
        SessionKey::new("fleet", "controller-1")
    }

    /// A fleet with one dep-free (immediately `Ready`) child.
    async fn fleet_with_ready_child(store: &FleetKernelStore, budget: u64) {
        store
            .create_fleet("f1", controller(), None, "default", budget, false, 0)
            .await
            .unwrap();
        store.add_child("f1", "c1", vec![], 0).await.unwrap();
    }

    fn accepted() -> AcceptanceVerdict {
        AcceptanceVerdict::Accepted {
            evidence: vec![EvidenceRef {
                kind: "file".into(),
                locator: "out.txt".into(),
                sha256: "abc123".into(),
                captured_at_ms: 5,
            }],
        }
    }

    fn rejected() -> AcceptanceVerdict {
        AcceptanceVerdict::Rejected {
            reason: "did not pass acceptance".into(),
        }
    }

    fn snapshot() -> ChildResultSnapshot {
        ChildResultSnapshot {
            output: "done".into(),
            success: true,
            tokens_used: 80,
            files: vec!["out.txt".into()],
            error: None,
        }
    }

    fn plan_task(id: &str, deps: &[&str]) -> PlanTask {
        PlanTask {
            task_id: id.into(),
            title: id.into(),
            detail: String::new(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            acceptance: vec![],
            grant: crate::grant::WorkerGrant::minimal(),
        }
    }

    fn plan(fleet: &str, revision: u64, tasks: Vec<PlanTask>) -> DurablePlan {
        DurablePlan {
            schema_version: SCHEMA_VERSION,
            fleet_id: fleet.into(),
            revision,
            objective: "obj".into(),
            tasks,
        }
    }

    /// Launch + mark_running `c1`, returning the attempt id.
    async fn launch_running(store: &FleetKernelStore) -> String {
        let id = match store
            .launch_child("f1", "c1", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        store.mark_running("c1", &id).await.unwrap();
        id
    }

    #[tokio::test]
    async fn open_creates_tables_and_survives_reopen() {
        let dir = TempDir::new().unwrap();
        {
            let store = FleetKernelStore::open(dir.path()).await.unwrap();
            store
                .create_fleet("f1", controller(), None, "default", 500, false, 1)
                .await
                .unwrap();
            assert!(store.path().ends_with("fleet-kernel.redb"));
        }
        let reopened = FleetKernelStore::open(dir.path()).await.unwrap();
        let fleet = reopened.get_fleet("f1").await.unwrap().expect("present");
        assert_eq!(fleet.budget.token_budget, 500);
        assert_eq!(fleet.status, FleetStatus::Active);
        assert_eq!(fleet.generation, 0);
    }

    #[tokio::test]
    async fn create_fleet_rejects_duplicate() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 100, false, 0)
            .await
            .unwrap();
        assert!(
            store
                .create_fleet("f1", controller(), None, "default", 100, false, 0)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn create_plan_is_insert_only() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 100, false, 0)
            .await
            .unwrap();
        store.create_plan(plan("f1", 0, vec![])).await.unwrap();
        // A second create must not blindly overwrite (P1-2).
        assert!(store.create_plan(plan("f1", 0, vec![])).await.is_err());
    }

    #[tokio::test]
    async fn launch_happy_path_reserves_budget_and_creates_leased_attempt() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;

        let out = store
            .launch_child("f1", "c1", 100, 10, EPOCH, TTL)
            .await
            .unwrap();
        let attempt_id = match out {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Launching);
        assert_eq!(
            child.current_attempt_id.as_deref(),
            Some(attempt_id.as_str())
        );
        assert_eq!(child.attempts_used, 1);

        let attempt = store.get_attempt("c1", &attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.status, AttemptStatus::Leased);
        assert_eq!(attempt.fleet_id, "f1");
        assert_eq!(attempt.generation, 0);
        assert_eq!(attempt.reserved_tokens, 100);
        assert_eq!(attempt.lease.owner_epoch, EPOCH);
        assert_eq!(attempt.lease.expires_at_ms, 10 + TTL);

        let fleet = store.get_fleet("f1").await.unwrap().unwrap();
        assert_eq!(fleet.budget.tokens_reserved, 100);
        assert_eq!(fleet.budget.tokens_committed, 0);

        // Outbox got a ChildLaunching event.
        let ev = store.claim_next("k", 0, 100).await.unwrap().unwrap();
        assert_eq!(ev.kind, FleetEventKind::ChildLaunching);
        assert_eq!(ev.child_id.as_deref(), Some("c1"));
        assert_eq!(ev.attempt_id.as_deref(), Some(attempt_id.as_str()));
    }

    #[tokio::test]
    async fn double_launch_is_rejected_and_does_not_double_reserve() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;

        let attempt_id = match store
            .launch_child("f1", "c1", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        let second = store
            .launch_child("f1", "c1", 100, 0, EPOCH, TTL)
            .await
            .unwrap();
        assert_eq!(second, LaunchOutcome::RejectedDoubleLaunch);

        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Launching);
        assert_eq!(
            child.current_attempt_id.as_deref(),
            Some(attempt_id.as_str())
        );
        assert_eq!(
            child.attempts_used, 1,
            "second launch must not bump the count"
        );

        let fleet = store.get_fleet("f1").await.unwrap().unwrap();
        assert_eq!(fleet.budget.tokens_reserved, 100, "no double reservation");
    }

    #[tokio::test]
    async fn launch_rejected_when_child_not_ready() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 1_000, false, 0)
            .await
            .unwrap();
        store
            .add_child("f1", "c1", vec!["missing".into()], 0)
            .await
            .unwrap();

        let out = store
            .launch_child("f1", "c1", 100, 0, EPOCH, TTL)
            .await
            .unwrap();
        assert_eq!(out, LaunchOutcome::RejectedNotReady);

        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Planned);
        assert!(child.current_attempt_id.is_none());
    }

    #[tokio::test]
    async fn budget_exceeded_is_rejected_and_child_stays_ready() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 50).await;

        let out = store
            .launch_child("f1", "c1", 100, 0, EPOCH, TTL)
            .await
            .unwrap();
        assert_eq!(out, LaunchOutcome::RejectedBudgetExceeded);

        // Crucially: the child is NOT left Launching (spec §6).
        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Ready);
        assert!(child.current_attempt_id.is_none());
        assert_eq!(child.attempts_used, 0);

        let fleet = store.get_fleet("f1").await.unwrap().unwrap();
        assert_eq!(fleet.budget.tokens_reserved, 0);

        // No attempt, no outbox event were written.
        assert!(store.claim_next("k", 0, 100).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn budget_admits_exactly_at_the_cap() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 100).await;
        let out = store
            .launch_child("f1", "c1", 100, 0, EPOCH, TTL)
            .await
            .unwrap();
        assert!(matches!(out, LaunchOutcome::Launched { .. }));
    }

    #[tokio::test]
    async fn mark_running_sets_attempt_and_child_running_then_refuses_repeat() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = match store
            .launch_child("f1", "c1", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };

        store.mark_running("c1", &attempt_id).await.unwrap();
        assert_eq!(
            store
                .get_attempt("c1", &attempt_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            AttemptStatus::Running
        );
        // P2-6: the child reflects Running, not a stuck Launching.
        assert_eq!(
            store.get_child("f1", "c1").await.unwrap().unwrap().status,
            ChildStatus::Running
        );

        // A repeat (child now Running, not Launching) and an unknown attempt
        // are BOTH lost-race misses now — `Superseded`, not an infra `Err`.
        assert_eq!(
            store.mark_running("c1", &attempt_id).await.unwrap(),
            MarkRunningOutcome::Superseded,
        );
        assert_eq!(
            store.mark_running("c1", "nope").await.unwrap(),
            MarkRunningOutcome::Superseded,
        );
    }

    #[tokio::test]
    async fn complete_happy_path_settles_budget_and_appends_outbox() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = launch_running(&store).await;

        let out = store
            .complete_child(
                "f1",
                "c1",
                &attempt_id,
                accepted(),
                snapshot(),
                80,
                EPOCH,
                20,
            )
            .await
            .unwrap();
        assert_eq!(out, CompleteOutcome::Completed);

        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Succeeded);
        assert!(matches!(
            child.outcome,
            Some(AcceptanceVerdict::Accepted { .. })
        ));
        assert_eq!(child.tokens_committed, 80);

        let attempt = store.get_attempt("c1", &attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.status, AttemptStatus::Done);
        assert!(attempt.result_snapshot.is_some());
        assert_eq!(attempt.ended_at_ms, Some(20));

        let fleet = store.get_fleet("f1").await.unwrap().unwrap();
        assert_eq!(fleet.budget.tokens_committed, 80);
        assert_eq!(fleet.budget.tokens_reserved, 0, "reservation released");

        // Outbox: ChildLaunching (seq 1) then ChildDone (seq 2).
        let e1 = store.claim_next("k", 0, 100).await.unwrap().unwrap();
        assert_eq!(e1.kind, FleetEventKind::ChildLaunching);
        let e2 = store.claim_next("k", 0, 100).await.unwrap().unwrap();
        assert_eq!(e2.kind, FleetEventKind::ChildDone);
        assert_eq!(e2.sequence, 2);
    }

    fn escalation_request() -> EscalationRequest {
        EscalationRequest {
            requested_grant: crate::grant::WorkerGrant {
                network: crate::grant::NetworkGrant::Hosts(vec!["example.com".into()]),
                tools: vec!["read_file".into(), "web_fetch".into()],
                ..crate::grant::WorkerGrant::minimal()
            },
            reason: "needs example.com to fetch the report".into(),
        }
    }

    #[tokio::test]
    async fn record_escalation_blocks_child_non_terminally_and_emits_childdone() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = launch_running(&store).await;

        // The yielded attempt used 80 REAL tokens — they must be committed
        // (NOT 0), so a fresh attempt after the grant widen has an honest budget.
        let out = store
            .record_escalation("f1", "c1", &attempt_id, escalation_request(), 80, EPOCH, 20)
            .await
            .unwrap();
        assert_eq!(out, CompleteOutcome::Completed);

        // Child is Blocked (NON-terminal) with the pending request; no verdict.
        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Blocked);
        assert!(!child.status.is_terminal(), "Blocked must be non-terminal");
        assert!(
            child.outcome.is_none(),
            "a Blocked child has no verdict yet"
        );
        assert_eq!(
            child.pending_escalation.as_ref().map(|e| e.reason.as_str()),
            Some("needs example.com to fetch the report"),
        );
        assert_eq!(
            child.current_attempt_id, None,
            "the yielded attempt is cleared so Blocked→Ready can relaunch clean",
        );
        assert_eq!(child.tokens_committed, 80);

        // The attempt is Interrupted (it yielded, did not complete).
        let attempt = store.get_attempt("c1", &attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.status, AttemptStatus::Interrupted);
        assert_eq!(attempt.ended_at_ms, Some(20));

        // Budget: REAL tokens committed (not 0), reservation released.
        let fleet = store.get_fleet("f1").await.unwrap().unwrap();
        assert_eq!(
            fleet.budget.tokens_committed, 80,
            "record_escalation must settle the REAL tokens used, never 0",
        );
        assert_eq!(fleet.budget.tokens_reserved, 0, "reservation released");

        // Outbox: ChildLaunching (seq 1) then the SAME ChildDone wake (seq 2).
        let e1 = store.claim_next("k", 0, 100).await.unwrap().unwrap();
        assert_eq!(e1.kind, FleetEventKind::ChildLaunching);
        let e2 = store.claim_next("k", 0, 100).await.unwrap().unwrap();
        assert_eq!(
            e2.kind,
            FleetEventKind::ChildDone,
            "escalation fires the EXISTING ChildDone wake — no new machinery",
        );
    }

    #[tokio::test]
    async fn record_escalation_is_a_no_op_for_a_stale_attempt() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = launch_running(&store).await;

        // Wrong attempt id → the four-part fence rejects it (no state change).
        let bogus = store
            .record_escalation(
                "f1",
                "c1",
                "not-the-attempt",
                escalation_request(),
                80,
                EPOCH,
                5,
            )
            .await
            .unwrap();
        assert_eq!(bogus, CompleteOutcome::Superseded);
        // A foreign owner_epoch (a superseded lease) is likewise a no-op.
        let stale_lease = store
            .record_escalation(
                "f1",
                "c1",
                &attempt_id,
                escalation_request(),
                80,
                EPOCH + 1,
                5,
            )
            .await
            .unwrap();
        assert_eq!(stale_lease, CompleteOutcome::Superseded);

        // The child is untouched (still Running, no escalation, no commit).
        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Running);
        assert!(child.pending_escalation.is_none());
        assert_eq!(child.tokens_committed, 0);
        let fleet = store.get_fleet("f1").await.unwrap().unwrap();
        assert_eq!(fleet.budget.tokens_committed, 0);
    }

    #[tokio::test]
    async fn escalating_a_legacy_v2_child_restamps_to_v3() {
        // codex round-3 (defect 3): a LEGACY v2 child row (written before the v3
        // upgrade) that escalates must be RE-STAMPED to the current schema on
        // write. `Blocked` is a v3-only enum variant; without the re-stamp the row
        // would persist `{schema_version: 2, status: "Blocked"}`, and a rolled-back
        // v2 binary (seeing `2 <= 2`) would attempt a full decode and ERROR on the
        // unknown `Blocked` variant instead of dropping it as newer. `decode_row`
        // only drops rows whose version EXCEEDS the binary's — so a `Blocked` row
        // must be v3 for an older binary to drop it cleanly.
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = launch_running(&store).await;

        // Plant the child row at the LEGACY v2 schema (as the pre-upgrade binary
        // would have written it), preserving its live Running state + attempt so
        // the four-part escalation fence still passes.
        let mut legacy = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(
            legacy.schema_version, SCHEMA_VERSION,
            "sanity: a freshly launched child is stamped at the current schema",
        );
        legacy.schema_version = 2;
        store.write_raw_child(&legacy).await.unwrap();

        // Escalate — the write MUST re-stamp the row to the current schema.
        let out = store
            .record_escalation("f1", "c1", &attempt_id, escalation_request(), 80, EPOCH, 20)
            .await
            .unwrap();
        assert_eq!(out, CompleteOutcome::Completed);

        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Blocked);
        assert_eq!(
            child.schema_version, SCHEMA_VERSION,
            "a legacy v2 row carrying the new `Blocked` variant must be re-stamped \
             v3 — so an older binary drops it as newer rather than erroring on the \
             unknown variant",
        );
    }

    #[tokio::test]
    async fn complete_rejected_for_wrong_attempt_id_no_state_change() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = launch_running(&store).await;

        let out = store
            .complete_child(
                "f1",
                "c1",
                "bogus-attempt",
                accepted(),
                snapshot(),
                80,
                EPOCH,
                20,
            )
            .await
            .unwrap();
        assert_eq!(out, CompleteOutcome::Superseded);

        // Real attempt + child + budget untouched; child is Running (P2-6).
        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Running);
        let attempt = store.get_attempt("c1", &attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.status, AttemptStatus::Running);
        let fleet = store.get_fleet("f1").await.unwrap().unwrap();
        assert_eq!(fleet.budget.tokens_committed, 0);
        assert_eq!(fleet.budget.tokens_reserved, 100);
    }

    #[tokio::test]
    async fn complete_rejected_for_wrong_owner_epoch() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = launch_running(&store).await;

        let out = store
            .complete_child(
                "f1",
                "c1",
                &attempt_id,
                accepted(),
                snapshot(),
                80,
                EPOCH + 1,
                20,
            )
            .await
            .unwrap();
        assert_eq!(out, CompleteOutcome::Superseded);
        assert_eq!(
            store
                .get_attempt("c1", &attempt_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            AttemptStatus::Running
        );
    }

    #[tokio::test]
    async fn complete_rejected_when_not_running() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = match store
            .launch_child("f1", "c1", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        // Never marked Running -> still Leased -> predicate fails.
        let out = store
            .complete_child(
                "f1",
                "c1",
                &attempt_id,
                accepted(),
                snapshot(),
                80,
                EPOCH,
                20,
            )
            .await
            .unwrap();
        assert_eq!(out, CompleteOutcome::Superseded);
    }

    #[tokio::test]
    async fn complete_rejected_for_stale_generation() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = launch_running(&store).await;

        // Simulate a re-plan bumping the fleet generation while the
        // attempt keeps its stamped generation 0.
        let mut fleet = store.get_fleet("f1").await.unwrap().unwrap();
        fleet.generation = 1;
        store
            .write_raw_fleet("f1", &serde_json::to_string(&fleet).unwrap())
            .await
            .unwrap();

        let out = store
            .complete_child(
                "f1",
                "c1",
                &attempt_id,
                accepted(),
                snapshot(),
                80,
                EPOCH,
                20,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            CompleteOutcome::Superseded,
            "generation fences acceptance"
        );
    }

    #[tokio::test]
    async fn reconcile_reclaims_foreign_owner_epoch() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = launch_running(&store).await;

        let report = store.reconcile(1, EPOCH + 999).await.unwrap();
        assert_eq!(report.interrupted.len(), 1);
        assert_eq!(report.interrupted[0].child_id, "c1");
        assert_eq!(report.interrupted[0].attempt_id, attempt_id);

        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Ready);
        assert!(
            child.current_attempt_id.is_none(),
            "current_attempt_id cleared"
        );
        let attempt = store.get_attempt("c1", &attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.status, AttemptStatus::Interrupted);
    }

    /// P1-1: an interrupted attempt's reservation must be released, or
    /// the fleet is budget-locked forever.
    #[tokio::test]
    async fn reconcile_releases_interrupted_reservation_so_relaunch_is_admitted() {
        let (_d, store) = fresh().await;
        // Budget fits exactly one 100-token attempt.
        fleet_with_ready_child(&store, 100).await;
        let _first = launch_running(&store).await;
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

        store.reconcile(1, EPOCH + 999).await.unwrap();
        assert_eq!(
            store
                .get_fleet("f1")
                .await
                .unwrap()
                .unwrap()
                .budget
                .tokens_reserved,
            0,
            "reservation released on interrupt"
        );

        // The freed budget lets the relaunch in.
        let out = store
            .launch_child("f1", "c1", 100, 2, EPOCH, TTL)
            .await
            .unwrap();
        assert!(matches!(out, LaunchOutcome::Launched { .. }));
    }

    #[tokio::test]
    async fn reconcile_reclaims_expired_lease_same_epoch() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = match store
            .launch_child("f1", "c1", 100, 0, EPOCH, 10)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        store.mark_running("c1", &attempt_id).await.unwrap();

        let report = store.reconcile(1_000, EPOCH).await.unwrap();
        assert_eq!(report.interrupted.len(), 1);
        assert_eq!(
            store.get_child("f1", "c1").await.unwrap().unwrap().status,
            ChildStatus::Ready
        );
    }

    #[tokio::test]
    async fn reconcile_leaves_healthy_lease_untouched() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let _attempt_id = launch_running(&store).await;

        let report = store.reconcile(500, EPOCH).await.unwrap();
        assert!(report.interrupted.is_empty());
        // Child was moved to Running by mark_running and is left as-is.
        assert_eq!(
            store.get_child("f1", "c1").await.unwrap().unwrap().status,
            ChildStatus::Running
        );
    }

    /// P2-7: a Cancelled fleet's stranded child must not be resurrected.
    #[tokio::test]
    async fn reconcile_skips_children_of_cancelled_fleet() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let attempt_id = launch_running(&store).await;

        // Cancel the fleet out-of-band.
        let mut fleet = store.get_fleet("f1").await.unwrap().unwrap();
        fleet.status = FleetStatus::Cancelled;
        store
            .write_raw_fleet("f1", &serde_json::to_string(&fleet).unwrap())
            .await
            .unwrap();

        let report = store.reconcile(9_999, EPOCH + 999).await.unwrap();
        assert!(report.interrupted.is_empty(), "cancelled fleet is skipped");
        // Child + attempt left as they were (not reset to Ready).
        assert_eq!(
            store.get_child("f1", "c1").await.unwrap().unwrap().status,
            ChildStatus::Running
        );
        assert_eq!(
            store
                .get_attempt("c1", &attempt_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            AttemptStatus::Running
        );
    }

    #[tokio::test]
    async fn reconcile_then_relaunch_gets_fresh_attempt() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let first = match store
            .launch_child("f1", "c1", 100, 0, EPOCH, 10)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        store.mark_running("c1", &first).await.unwrap();
        store.reconcile(1_000, EPOCH).await.unwrap();

        let second = match store
            .launch_child("f1", "c1", 100, 2_000, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        assert_ne!(first, second);
        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(child.current_attempt_id.as_deref(), Some(second.as_str()));
        assert_eq!(child.attempts_used, 2);
    }

    /// P1-2 (core): replan interrupts a surviving running attempt,
    /// releases its reservation (not stuck), recomputes the survivor to a
    /// relaunchable state, cancels the removed task, and adds the new one.
    #[tokio::test]
    async fn replan_interrupts_surviving_running_attempt_and_frees_reservation() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 100, false, 0)
            .await
            .unwrap();
        store
            .create_plan(plan(
                "f1",
                0,
                vec![plan_task("keep", &[]), plan_task("drop", &[])],
            ))
            .await
            .unwrap();
        store.add_child("f1", "keep", vec![], 0).await.unwrap();
        store.add_child("f1", "drop", vec![], 0).await.unwrap();

        // Launch + run "keep" (reserves the whole budget).
        let keep_att = match store
            .launch_child("f1", "keep", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        store.mark_running("keep", &keep_att).await.unwrap();
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

        // Re-plan: keep "keep", drop "drop", add "new".
        let out = store
            .replan(
                "f1",
                0,
                plan("f1", 0, vec![plan_task("keep", &[]), plan_task("new", &[])]),
                5,
            )
            .await
            .unwrap();
        assert_eq!(out, PlanMutateOutcome::Mutated { revision: 1 });
        assert_eq!(store.get_fleet("f1").await.unwrap().unwrap().generation, 1);
        assert_eq!(store.get_plan("f1").await.unwrap().unwrap().revision, 1);

        // Surviving child's old attempt Interrupted + reservation freed +
        // child recomputed to a relaunchable state (not stuck).
        assert_eq!(
            store
                .get_attempt("keep", &keep_att)
                .await
                .unwrap()
                .unwrap()
                .status,
            AttemptStatus::Interrupted
        );
        let keep = store.get_child("f1", "keep").await.unwrap().unwrap();
        assert_eq!(keep.generation, 1);
        assert_eq!(keep.status, ChildStatus::Ready, "recomputed runnable");
        assert!(
            keep.current_attempt_id.is_none(),
            "not stuck on old attempt"
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
            "reservation released, not stuck"
        );

        // Removed cancelled; new added.
        assert_eq!(
            store.get_child("f1", "drop").await.unwrap().unwrap().status,
            ChildStatus::Cancelled
        );
        assert!(store.get_child("f1", "new").await.unwrap().is_some());

        // Stale attempt cannot complete; the survivor relaunches.
        assert_eq!(
            store
                .complete_child(
                    "f1",
                    "keep",
                    &keep_att,
                    accepted(),
                    snapshot(),
                    80,
                    EPOCH,
                    9
                )
                .await
                .unwrap(),
            CompleteOutcome::Superseded
        );
        assert!(matches!(
            store
                .launch_child("f1", "keep", 100, 10, EPOCH, TTL)
                .await
                .unwrap(),
            LaunchOutcome::Launched { .. }
        ));
    }

    /// P1-2: a survivor that gains a new *unmet* dependency is demoted to
    /// Planned, so it cannot launch out of order.
    #[tokio::test]
    async fn replan_survivor_with_new_unmet_dep_becomes_planned() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 1_000, false, 0)
            .await
            .unwrap();
        store
            .create_plan(plan(
                "f1",
                0,
                vec![plan_task("a", &[]), plan_task("b", &[])],
            ))
            .await
            .unwrap();
        store.add_child("f1", "a", vec![], 0).await.unwrap();
        store.add_child("f1", "b", vec![], 0).await.unwrap();
        assert_eq!(
            store.get_child("f1", "b").await.unwrap().unwrap().status,
            ChildStatus::Ready
        );

        // Re-plan: b now depends on the not-yet-Succeeded a.
        store
            .replan(
                "f1",
                0,
                plan("f1", 0, vec![plan_task("a", &[]), plan_task("b", &["a"])]),
                1,
            )
            .await
            .unwrap();
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Ready
        );
        assert_eq!(
            store.get_child("f1", "b").await.unwrap().unwrap().status,
            ChildStatus::Planned,
            "unmet new dep demotes the survivor"
        );
    }

    /// P1-2: a removed child that had a live attempt releases its
    /// reservation (not leaked) and is Cancelled.
    #[tokio::test]
    async fn replan_removed_live_child_releases_reservation() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 100, false, 0)
            .await
            .unwrap();
        store
            .create_plan(plan("f1", 0, vec![plan_task("gone", &[])]))
            .await
            .unwrap();
        store.add_child("f1", "gone", vec![], 0).await.unwrap();
        let att = match store
            .launch_child("f1", "gone", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };

        store
            .replan("f1", 0, plan("f1", 0, vec![]), 5)
            .await
            .unwrap();

        assert_eq!(
            store.get_child("f1", "gone").await.unwrap().unwrap().status,
            ChildStatus::Cancelled
        );
        assert_eq!(
            store
                .get_attempt("gone", &att)
                .await
                .unwrap()
                .unwrap()
                .status,
            AttemptStatus::Interrupted
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
            "removed child's reservation released"
        );
    }

    /// P1-2: a task removed (→ Cancelled) then re-added by a later replan
    /// is resurrected as a fresh runnable child, not left Cancelled.
    #[tokio::test]
    async fn replan_readds_removed_task_as_fresh_child() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 100, false, 0)
            .await
            .unwrap();
        store
            .create_plan(plan("f1", 0, vec![plan_task("x", &[])]))
            .await
            .unwrap();
        store.add_child("f1", "x", vec![], 0).await.unwrap();

        // Remove x (rev 0 -> 1): Cancelled.
        store
            .replan("f1", 0, plan("f1", 0, vec![]), 1)
            .await
            .unwrap();
        assert_eq!(
            store.get_child("f1", "x").await.unwrap().unwrap().status,
            ChildStatus::Cancelled
        );

        // Re-add x (rev 1 -> 2): fresh Ready child, outcome cleared.
        store
            .replan("f1", 1, plan("f1", 0, vec![plan_task("x", &[])]), 2)
            .await
            .unwrap();
        let x = store.get_child("f1", "x").await.unwrap().unwrap();
        assert_eq!(x.status, ChildStatus::Ready, "resurrected");
        assert!(x.outcome.is_none(), "fresh outcome");
        assert_eq!(x.generation, 2);
    }

    /// P1 (normal-path retry): a `Failed` survivor's Done attempt-id must
    /// be cleared by replan, or `launch_child` rejects the retry as a
    /// double-launch and the task can never run again.
    #[tokio::test]
    async fn replan_clears_pointer_so_a_failed_survivor_can_retry() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        store
            .create_plan(plan("f1", 0, vec![plan_task("c1", &[])]))
            .await
            .unwrap();

        // Drive c1 to Failed; complete keeps current_attempt_id set.
        let att = launch_running(&store).await;
        store
            .complete_child("f1", "c1", &att, rejected(), snapshot(), 50, EPOCH, 5)
            .await
            .unwrap();
        let failed = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(failed.status, ChildStatus::Failed);
        assert!(
            failed.current_attempt_id.is_some(),
            "Failed child retains its Done attempt pointer"
        );

        // Replan keeping c1 → recomputed Ready with the pointer cleared.
        store
            .replan("f1", 0, plan("f1", 0, vec![plan_task("c1", &[])]), 6)
            .await
            .unwrap();
        let c1 = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(c1.status, ChildStatus::Ready);
        assert!(c1.current_attempt_id.is_none(), "stale pointer cleared");

        // The retry now launches instead of RejectedDoubleLaunch.
        assert!(matches!(
            store
                .launch_child("f1", "c1", 100, 10, EPOCH, TTL)
                .await
                .unwrap(),
            LaunchOutcome::Launched { .. }
        ));
    }

    /// P1-4: an attempt row whose stored `attempt_id` differs from the key
    /// it lives under cannot be completed under the key's id.
    #[tokio::test]
    async fn complete_rejects_attempt_with_mismatched_stored_id() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;

        // Plant a legal-schema Running attempt under key `c1\0A` whose body
        // claims attempt_id == "B".
        let bad = Attempt {
            schema_version: SCHEMA_VERSION,
            fleet_id: "f1".into(),
            child_id: "c1".into(),
            attempt_id: "B".into(),
            generation: 0,
            status: AttemptStatus::Running,
            lease: Lease {
                owner_epoch: EPOCH,
                expires_at_ms: TTL,
            },
            reserved_tokens: 0,
            result_snapshot: None,
            started_at_ms: 0,
            ended_at_ms: None,
        };
        store.write_raw_attempt("c1", "A", &bad).await.unwrap();
        // Point the child at "A" so only the own-id check can reject.
        let mut child = store.get_child("f1", "c1").await.unwrap().unwrap();
        child.status = ChildStatus::Running;
        child.current_attempt_id = Some("A".into());
        store.write_raw_child(&child).await.unwrap();

        let out = store
            .complete_child("f1", "c1", "A", accepted(), snapshot(), 10, EPOCH, 1)
            .await
            .unwrap();
        assert_eq!(
            out,
            CompleteOutcome::Superseded,
            "an attempt whose stored id != key is not completed under the key's id"
        );
        assert_eq!(
            store.get_child("f1", "c1").await.unwrap().unwrap().status,
            ChildStatus::Running,
            "child was not completed"
        );
    }

    /// P2-6: mark_running writes NOTHING when the attempt is not the
    /// child's current attempt.
    #[tokio::test]
    async fn mark_running_no_mutation_when_child_current_attempt_differs() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let att = match store
            .launch_child("f1", "c1", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        // Point the child at a ghost attempt while `att` stays Leased.
        let mut child = store.get_child("f1", "c1").await.unwrap().unwrap();
        child.current_attempt_id = Some("ghost".into());
        store.write_raw_child(&child).await.unwrap();

        // Not the child's current attempt → a lost-race `Superseded`, not `Err`.
        assert_eq!(
            store.mark_running("c1", &att).await.unwrap(),
            MarkRunningOutcome::Superseded,
        );
        // Zero mutation: attempt still Leased, child pointer unchanged.
        assert_eq!(
            store.get_attempt("c1", &att).await.unwrap().unwrap().status,
            AttemptStatus::Leased
        );
        assert_eq!(
            store
                .get_child("f1", "c1")
                .await
                .unwrap()
                .unwrap()
                .current_attempt_id
                .as_deref(),
            Some("ghost")
        );
    }

    /// P2-6: mark_running rejects (zero mutation) an attempt whose
    /// generation is stale relative to the fleet.
    #[tokio::test]
    async fn mark_running_rejected_when_attempt_generation_is_stale() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        let att = match store
            .launch_child("f1", "c1", 100, 0, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        // Bump the fleet generation out from under the Leased attempt.
        let mut fleet = store.get_fleet("f1").await.unwrap().unwrap();
        fleet.generation = 1;
        store
            .write_raw_fleet("f1", &serde_json::to_string(&fleet).unwrap())
            .await
            .unwrap();

        // Stale generation → a lost-race `Superseded`, not an infra `Err`.
        assert_eq!(
            store.mark_running("c1", &att).await.unwrap(),
            MarkRunningOutcome::Superseded,
        );
        assert_eq!(
            store.get_attempt("c1", &att).await.unwrap().unwrap().status,
            AttemptStatus::Leased,
            "no mutation on stale generation"
        );
    }

    /// P1-4: a public outbox `event_id` with a control char is rejected.
    #[tokio::test]
    async fn append_event_rejects_control_char_event_id() {
        let (_d, store) = fresh().await;
        let mut ev = outbox_event(FleetEventKind::FleetDrained, "f1", None, None);
        ev.event_id = "bad\u{7f}id".into();
        assert!(store.append_event(ev).await.is_err());
    }

    /// P2-5: a launch whose lease expiry (`now + ttl`) would overflow is
    /// rejected, not silently capped at u64::MAX (which would never
    /// reconcile → leaked reservation).
    #[tokio::test]
    async fn launch_rejected_when_lease_expiry_would_overflow() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, 1_000).await;
        assert!(
            store
                .launch_child("f1", "c1", 1, u64::MAX - 1, EPOCH, 2)
                .await
                .is_err()
        );
        // Nothing committed: child still Ready, budget untouched.
        assert_eq!(
            store.get_child("f1", "c1").await.unwrap().unwrap().status,
            ChildStatus::Ready
        );
        assert_eq!(
            store
                .get_fleet("f1")
                .await
                .unwrap()
                .unwrap()
                .budget
                .tokens_reserved,
            0
        );
    }

    #[tokio::test]
    async fn replan_is_revision_fenced() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 100, false, 0)
            .await
            .unwrap();
        store.create_plan(plan("f1", 0, vec![])).await.unwrap();

        // Correct revision advances to 1.
        let out = store
            .replan("f1", 0, plan("f1", 0, vec![plan_task("t1", &[])]), 1)
            .await
            .unwrap();
        assert_eq!(out, PlanMutateOutcome::Mutated { revision: 1 });

        // Stale expected revision (0) -> rejected, nothing changes.
        let stale = store
            .replan("f1", 0, plan("f1", 0, vec![]), 2)
            .await
            .unwrap();
        assert_eq!(stale, PlanMutateOutcome::RevisionMismatch { actual: 1 });
        let p = store.get_plan("f1").await.unwrap().unwrap();
        assert_eq!(p.revision, 1);
        assert_eq!(p.tasks.len(), 1, "stale replan left the plan unchanged");
        assert_eq!(store.get_fleet("f1").await.unwrap().unwrap().generation, 1);
    }

    #[tokio::test]
    async fn add_child_and_mark_ready_resolve_dependencies() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 1_000, false, 0)
            .await
            .unwrap();
        store.add_child("f1", "a", vec![], 0).await.unwrap();
        store
            .add_child("f1", "b", vec!["a".into()], 0)
            .await
            .unwrap();

        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Ready
        );
        assert_eq!(
            store.get_child("f1", "b").await.unwrap().unwrap().status,
            ChildStatus::Planned
        );
        assert!(!store.mark_ready("f1", "b", 1).await.unwrap());

        // Drive a to Succeeded.
        let a_att = match store
            .launch_child("f1", "a", 10, 1, EPOCH, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        store.mark_running("a", &a_att).await.unwrap();
        store
            .complete_child("f1", "a", &a_att, accepted(), snapshot(), 10, EPOCH, 2)
            .await
            .unwrap();

        assert!(store.mark_ready("f1", "b", 3).await.unwrap());
        assert_eq!(
            store.get_child("f1", "b").await.unwrap().unwrap().status,
            ChildStatus::Ready
        );
        assert!(!store.mark_ready("f1", "b", 4).await.unwrap());
    }

    #[tokio::test]
    async fn list_children_scopes_to_fleet_and_sorts() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 100, false, 0)
            .await
            .unwrap();
        store
            .create_fleet("f2", controller(), None, "default", 100, false, 0)
            .await
            .unwrap();
        store.add_child("f1", "b", vec![], 0).await.unwrap();
        store.add_child("f1", "a", vec![], 0).await.unwrap();
        store.add_child("f2", "z", vec![], 0).await.unwrap();

        let f1 = store.list_children("f1").await.unwrap();
        assert_eq!(
            f1.iter().map(|c| c.child_id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let f2 = store.list_children("f2").await.unwrap();
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].child_id, "z");
    }

    #[tokio::test]
    async fn outbox_append_claim_ack_and_expiry_reclaim() {
        let (_d, store) = fresh().await;
        let base = outbox_event(FleetEventKind::FleetDrained, "f1", None, None);
        let s1 = store.append_event(base.clone()).await.unwrap();
        let s2 = store.append_event(base.clone()).await.unwrap();
        assert_eq!((s1, s2), (1, 2));

        let c1 = store.claim_next("worker", 0, 100).await.unwrap().unwrap();
        assert_eq!(c1.sequence, 1);
        assert_eq!(c1.claimed_by.as_deref(), Some("worker"));
        assert!(c1.claim_token.is_some());
        let c2 = store.claim_next("worker", 0, 100).await.unwrap().unwrap();
        assert_eq!(c2.sequence, 2);
        assert!(store.claim_next("worker", 0, 100).await.unwrap().is_none());

        // Ack seq 1 with the right claim, then reclaim seq 2 after expiry.
        assert_eq!(
            store
                .ack(1, "worker", c1.claim_token.as_deref().unwrap())
                .await
                .unwrap(),
            AckOutcome::Acked
        );
        assert!(
            store.claim_next("worker", 50, 100).await.unwrap().is_none(),
            "seq1 acked, seq2 claim still valid"
        );
        let reclaimed = store
            .claim_next("worker2", 200, 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reclaimed.sequence, 2, "expired claim on seq2 reclaimed");

        assert!(
            store.ack(999, "worker", "x").await.is_err(),
            "ack of missing seq errors"
        );
    }

    /// P1-3: a stale consumer whose lease expired and was reclaimed
    /// cannot ack the event out from under the new owner.
    #[tokio::test]
    async fn ack_is_fenced_against_a_stale_reclaimed_consumer() {
        let (_d, store) = fresh().await;
        store
            .append_event(outbox_event(FleetEventKind::FleetDrained, "f1", None, None))
            .await
            .unwrap();

        // A claims, its lease expires, B reclaims with a fresh token.
        let a = store.claim_next("A", 0, 100).await.unwrap().unwrap();
        let b = store.claim_next("B", 200, 100).await.unwrap().unwrap();
        assert_eq!(a.sequence, b.sequence);
        assert_ne!(a.claim_token, b.claim_token);

        // A's stale ack is rejected; B's ack wins.
        assert_eq!(
            store
                .ack(1, "A", a.claim_token.as_deref().unwrap())
                .await
                .unwrap(),
            AckOutcome::StaleClaim
        );
        assert_eq!(
            store
                .ack(1, "B", b.claim_token.as_deref().unwrap())
                .await
                .unwrap(),
            AckOutcome::Acked
        );
    }

    #[tokio::test]
    async fn decision_log_appends_per_fleet_in_order() {
        let (_d, store) = fresh().await;
        let s1 = store
            .append_decision("f1", "keeper", DecisionKind::Plan, "planned", 1)
            .await
            .unwrap();
        let s2 = store
            .append_decision("f1", "keeper", DecisionKind::Launch, "launched c1", 2)
            .await
            .unwrap();
        let s_other = store
            .append_decision("f2", "keeper", DecisionKind::Plan, "planned", 1)
            .await
            .unwrap();
        assert_eq!((s1, s2, s_other), (1, 2, 1));

        let log = store.list_decisions("f1").await.unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].seq, 1);
        assert_eq!(log[0].kind, DecisionKind::Plan);
        assert_eq!(log[1].kind, DecisionKind::Launch);
    }

    #[tokio::test]
    async fn load_drops_row_with_higher_schema_version() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f1", controller(), None, "default", 100, false, 0)
            .await
            .unwrap();
        let raw = r#"{"schema_version":4,"fleet_id":"f1","controller_session_key":"fleet:controller-1","profile_id":"default","budget":{"token_budget":100,"tokens_reserved":0,"tokens_committed":0,"hard":false},"status":"Active","generation":0,"created_at_ms":0,"updated_at_ms":0}"#;
        store.write_raw_fleet("f1", raw).await.unwrap();
        assert!(store.get_fleet("f1").await.unwrap().is_none());
    }

    /// P1-4: control-char key components are rejected, so the two
    /// otherwise-colliding `(fleet, child)` encodings cannot cross-address.
    #[tokio::test]
    async fn key_components_reject_control_chars_preventing_collision() {
        let (_d, store) = fresh().await;
        store
            .create_fleet("f", controller(), None, "default", 100, false, 0)
            .await
            .unwrap();

        // `(fleet="f", child="x\0c")` and `(fleet="f\0x", child="c")`
        // would both encode to "f\0x\0c" under naive concatenation.
        assert!(store.add_child("f", "x\0c", vec![], 0).await.is_err());
        assert!(
            store
                .create_fleet("f\0x", controller(), None, "default", 100, false, 0)
                .await
                .is_err()
        );
        // Other control characters are rejected too.
        assert!(store.get_child("f", "c\n").await.is_err());
        assert!(
            store
                .launch_child("f", "\u{7f}", 1, 0, EPOCH, TTL)
                .await
                .is_err()
        );
    }

    /// P2-5: a launch whose checked reservation sum would overflow is
    /// rejected, not silently admitted by saturation.
    #[tokio::test]
    async fn launch_rejected_when_reservation_would_overflow() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store, u64::MAX).await;
        // Plant a fleet already reserving u64::MAX against a MAX budget.
        let mut fleet = store.get_fleet("f1").await.unwrap().unwrap();
        fleet.budget.tokens_reserved = u64::MAX;
        store
            .write_raw_fleet("f1", &serde_json::to_string(&fleet).unwrap())
            .await
            .unwrap();

        let out = store
            .launch_child("f1", "c1", 1, 0, EPOCH, TTL)
            .await
            .unwrap();
        assert_eq!(out, LaunchOutcome::RejectedBudgetExceeded);
    }

    #[tokio::test]
    async fn fleets_with_ready_children_finds_active_fleets_with_ready_tasks() {
        let (_d, store) = fresh().await;

        // Helper: plant a fleet then force its status (the public API mints
        // only `Active`; terminal transitions arrive in later PRs, so a test
        // reaches for the raw writer to exercise the boot-resume status filter).
        async fn force_status(store: &FleetKernelStore, fleet_id: &str, status: FleetStatus) {
            let mut f = store.get_fleet(fleet_id).await.unwrap().unwrap();
            f.status = status;
            store
                .write_raw_fleet(fleet_id, &serde_json::to_string(&f).unwrap())
                .await
                .unwrap();
        }
        // Helper: force a child terminal (Succeeded) without the full lifecycle.
        async fn force_succeeded(store: &FleetKernelStore, fleet_id: &str, child_id: &str) {
            let mut c = store.get_child(fleet_id, child_id).await.unwrap().unwrap();
            c.status = ChildStatus::Succeeded;
            c.current_attempt_id = None;
            store.write_raw_child(&c).await.unwrap();
        }

        // f-ready: an Active fleet with a dep-free (immediately Ready) child.
        store
            .create_fleet("f-ready", controller(), None, "default", 1_000, false, 0)
            .await
            .unwrap();
        store.add_child("f-ready", "c1", vec![], 0).await.unwrap();

        // f-draining: a live (Draining) fleet with a Ready child — also woken.
        store
            .create_fleet("f-draining", controller(), None, "default", 1_000, false, 0)
            .await
            .unwrap();
        store
            .add_child("f-draining", "c1", vec![], 0)
            .await
            .unwrap();
        force_status(&store, "f-draining", FleetStatus::Draining).await;

        // f-complete: a terminal fleet, even WITH a Ready child, is excluded by
        // the status filter (the fleet is done; it must not re-dispatch).
        store
            .create_fleet("f-complete", controller(), None, "default", 1_000, false, 0)
            .await
            .unwrap();
        store
            .add_child("f-complete", "c1", vec![], 0)
            .await
            .unwrap();
        force_status(&store, "f-complete", FleetStatus::Complete).await;

        // f-cancelled: likewise excluded by the status filter.
        store
            .create_fleet(
                "f-cancelled",
                controller(),
                None,
                "default",
                1_000,
                false,
                0,
            )
            .await
            .unwrap();
        store
            .add_child("f-cancelled", "c1", vec![], 0)
            .await
            .unwrap();
        force_status(&store, "f-cancelled", FleetStatus::Cancelled).await;

        // f-done: Active but every child terminal → empty ready set → excluded.
        store
            .create_fleet("f-done", controller(), None, "default", 1_000, false, 0)
            .await
            .unwrap();
        store.add_child("f-done", "c1", vec![], 0).await.unwrap();
        force_succeeded(&store, "f-done", "c1").await;

        // f-heal: the healed-promotion edge — c1 Succeeded (pre-crash) but its
        // dependent c2 never promoted (still Planned). `resolve_and_collect_ready`
        // promotes c2 in-txn, so the fleet IS detected as needing a wake.
        store
            .create_fleet("f-heal", controller(), None, "default", 1_000, false, 0)
            .await
            .unwrap();
        store.add_child("f-heal", "c1", vec![], 0).await.unwrap();
        store
            .add_child("f-heal", "c2", vec!["c1".into()], 0)
            .await
            .unwrap();
        force_succeeded(&store, "f-heal", "c1").await;
        assert_eq!(
            store
                .get_child("f-heal", "c2")
                .await
                .unwrap()
                .unwrap()
                .status,
            ChildStatus::Planned,
            "precondition: the dependent is still Planned (a missed promotion)"
        );

        let mut ids: Vec<String> = store
            .fleets_with_ready_children(9_999)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.fleet_id)
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "f-draining".to_string(),
                "f-heal".to_string(),
                "f-ready".to_string()
            ],
            "only live (Active|Draining) fleets with a ready/heal-promotable child are returned"
        );

        // The heal was applied in-txn: the missed dependent is now Ready.
        assert_eq!(
            store
                .get_child("f-heal", "c2")
                .await
                .unwrap()
                .unwrap()
                .status,
            ChildStatus::Ready,
            "resolve_and_collect_ready promoted the missed dependent in-txn"
        );
    }

    // ---- test-only raw writer ---------------------------------------------

    impl FleetKernelStore {
        /// Write a raw JSON string into the fleets table, bypassing
        /// record construction — used to plant higher-schema, cancelled,
        /// or bumped-generation/over-reserved rows the public API cannot
        /// mint.
        async fn write_raw_fleet(&self, fleet_id: &str, json: &str) -> Result<()> {
            let db = self.db.clone();
            let key = fleet_id.to_string();
            let json = json.to_string();
            let held = self.io_gate.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || -> Result<()> {
                let _held = held;
                let wtx = db.begin_write()?;
                {
                    let mut fleets = wtx.open_table(FLEETS)?;
                    fleets.insert(key.as_str(), json.as_str())?;
                }
                wtx.commit()?;
                Ok(())
            })
            .await
            .wrap_err("join write_raw_fleet")?
        }

        /// Write a child record verbatim (bypassing validation) so a test
        /// can plant a divergent `current_attempt_id`, etc.
        async fn write_raw_child(&self, child: &FleetChildRecord) -> Result<()> {
            let db = self.db.clone();
            let key = child_key(&child.fleet_id, &child.child_id);
            let json = serde_json::to_string(child)?;
            let held = self.io_gate.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || -> Result<()> {
                let _held = held;
                let wtx = db.begin_write()?;
                {
                    let mut children = wtx.open_table(FLEET_CHILDREN)?;
                    children.insert(key.as_str(), json.as_str())?;
                }
                wtx.commit()?;
                Ok(())
            })
            .await
            .wrap_err("join write_raw_child")?
        }

        /// Write an attempt record verbatim under an arbitrary key so a
        /// test can plant a row whose stored `attempt_id` differs from the
        /// `(child_id, attempt_id)` key it lives under.
        async fn write_raw_attempt(
            &self,
            child_id: &str,
            attempt_id: &str,
            att: &Attempt,
        ) -> Result<()> {
            let db = self.db.clone();
            let key = attempt_key(child_id, attempt_id);
            let json = serde_json::to_string(att)?;
            let held = self.io_gate.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || -> Result<()> {
                let _held = held;
                let wtx = db.begin_write()?;
                {
                    let mut attempts = wtx.open_table(ATTEMPTS)?;
                    attempts.insert(key.as_str(), json.as_str())?;
                }
                wtx.commit()?;
                Ok(())
            })
            .await
            .wrap_err("join write_raw_attempt")?
        }
    }
}
