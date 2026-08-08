//! Durable record types for the fleet kernel store.
//!
//! Every persisted record carries `schema_version` (currently
//! [`SCHEMA_VERSION`]). The store drops any row whose version is higher
//! than it understands (`Ok(None)` on load), so a newer daemon's rows are
//! opaque — never mis-parsed — to an older one. Records are plain serde
//! data: this crate has **zero** LLM / `octos-agent` dependency; the
//! executor, keeper, and outbox consumer live in later PRs.

use octos_core::SessionKey;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::grant::WorkerGrant;

/// Schema version stamped on every persisted record. Bumping this
/// invalidates prior rows the same way the swarm dispatch ledger does:
/// a load that sees a *higher* version returns `Ok(None)`.
///
/// Bumped 1 → 2 in PR 2: [`DurablePlan`]/[`PlanTask`] dropped the `state`
/// and `evidence` fields (the plan/child duality reconciliation). Stamping
/// v2 means a PR-1 binary drops a v2 row via the higher-version guard rather
/// than failing to deserialize the now-absent fields.
///
/// Bumped 2 → 3 in PR B (escalate-to-master): [`ChildStatus`] gained the
/// non-terminal `Blocked` variant. Unlike the `pending_escalation` /
/// `controller_workspace_root` field additions (forward-compatible via
/// `#[serde(default)]`, so NOT a bump), a NEW ENUM VARIANT is an *incompatible*
/// change: a v2 binary decoding a row whose `status` is `"Blocked"` would fail
/// with an unknown-variant error rather than gracefully dropping it. Stamping v3
/// means such a row loads as `Ok(None)` (higher-version guard) on an older
/// binary — dropped, never mis-parsed.
pub const SCHEMA_VERSION: u32 = 3;

// ---------------------------------------------------------------------------
// Status enums
// ---------------------------------------------------------------------------

/// Lifecycle of a fleet as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FleetStatus {
    Active,
    Draining,
    Complete,
    Failed,
    Cancelled,
}

/// Lifecycle of a single child (== one plan task in v1).
///
/// `Ready` means every dependency child is `Succeeded`. `Launching` and
/// `Running` are the two non-terminal in-flight states a live [`Attempt`]
/// backs; recovery reconciliation returns a stranded one to `Ready`.
///
/// `Blocked` (PR B) is a NON-terminal state a worker's attempt yields into when
/// it hits the edge of its [`crate::WorkerGrant`] and escalates: the yielded
/// attempt is already settled (its tokens committed), the child records a
/// [`FleetChildRecord::pending_escalation`], and it waits for the keeper's
/// operator decision. A `goal_grant` widens the grant and moves it `Blocked →
/// Ready` (a fresh attempt rebuilds from the wider grant); a `goal_deny` moves
/// it `Blocked → Failed` (terminal — a Blocked child must never wedge the fleet,
/// since `is_complete` never trips while one is non-terminal-but-unaccepted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChildStatus {
    Planned,
    Ready,
    Launching,
    Running,
    /// Non-terminal: the attempt yielded to request an operator grant widen
    /// ([`FleetChildRecord::pending_escalation`]); awaits `goal_grant` (→
    /// `Ready`) or `goal_deny` (→ `Failed`).
    Blocked,
    Succeeded,
    Failed,
    Cancelled,
}

impl ChildStatus {
    /// A child in a terminal state cannot launch or transition further
    /// (other than an explicit cancel, which is itself terminal). `Blocked`
    /// is deliberately NOT terminal: it is a pause awaiting an operator
    /// decision, so it holds `is_complete` open (the goal stays active) yet
    /// can still transition to `Ready` (granted) or `Failed` (denied).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ChildStatus::Succeeded | ChildStatus::Failed | ChildStatus::Cancelled
        )
    }
}

/// Lifecycle of one durable execution attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptStatus {
    Leased,
    Running,
    Done,
    Interrupted,
}

/// The worker kind backing a child. v1 ships only the stateless,
/// bounded, non-interactive one-shot; the interactive session-worker is
/// Phase 2b.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkerKind {
    #[default]
    StatelessTask,
}

// ---------------------------------------------------------------------------
// Fleet + budget
// ---------------------------------------------------------------------------

/// Durable, in-transaction budget. `hard=false` in v1 (soft admission
/// control: it rejects the *next* launch, it does not abort an in-flight
/// run). Reserved + committed are settled inside the same write-txn as
/// the launch/complete CAS, so there is no cross-store window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetBudget {
    pub token_budget: u64,
    pub tokens_reserved: u64,
    pub tokens_committed: u64,
    pub hard: bool,
}

impl FleetBudget {
    /// Would reserving `projected` more tokens keep us within budget?
    /// Uses `<=` (v1 wants inclusive admission, unlike `CostAccountant`'s
    /// strict `>`) on **checked** arithmetic: a sum that overflows `u64`
    /// can never fit a real budget, so it is *not* admitted — saturating
    /// math would silently admit `MAX + MAX + 1 <= MAX` (P2-5).
    pub fn admits(&self, projected: u64) -> bool {
        match self
            .tokens_reserved
            .checked_add(self.tokens_committed)
            .and_then(|s| s.checked_add(projected))
        {
            Some(total) => total <= self.token_budget,
            None => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRecord {
    pub schema_version: u32,
    pub fleet_id: String,
    /// Server-resolved; how a `FleetEvent` finds the keeper (later PRs).
    pub controller_session_key: SessionKey,
    /// The controller session's workspace root at fleet-create time. Persisted
    /// so a HEADLESS keeper (no live client) can be rehydrated across a serve
    /// restart: the outbox consumer carries it in the keeper-wake metadata and
    /// the global master-continuation drain re-seeds `session_workspaces()`
    /// from it before the workspace-known gate (PR 4b). `None` for a keeper
    /// whose workspace was never persisted (simply not headlessly
    /// rehydratable). `#[serde(default)]` is deliberate forward-compat, **NOT**
    /// a `SCHEMA_VERSION` bump: an older row missing the field deserializes to
    /// `None`, while a bump would make an older binary DROP the row (`decode_row`
    /// drops only `schema_version > SCHEMA_VERSION`). Mirrors the crate's
    /// existing discipline (`Attempt.fleet_id`, `OutboxEvent.payload`).
    ///
    /// Rolling-downgrade caveat: the field survives an older binary that only
    /// READS a row, but an older binary that REWRITES the record (it does not
    /// know this field) re-serializes it WITHOUT the root, erasing it — the
    /// keeper then loses headless rehydration. This is acceptable for v1: a
    /// single serve process is active at a time, so a live downgrade that
    /// rewrites records mid-flight is not an operational path. PR 5 must set a
    /// server-resolved root at fleet-create for this to carry a value at all.
    #[serde(default)]
    pub controller_workspace_root: Option<String>,
    /// Whether `controller_workspace_root` originated from a genuine runtime
    /// cwd hint. `Some(true)` means a restarted keeper may reuse the root as a
    /// transcript-relocation hint; `Some(false)` means it is the derived
    /// workspace used only by tools/UI. `None` is the legacy/unknown case and
    /// must be handled like `false` by readers (fail safe: never relocate).
    #[serde(default)]
    pub controller_workspace_has_runtime_hint: Option<bool>,
    pub profile_id: String,
    pub budget: FleetBudget,
    pub status: FleetStatus,
    /// Membership epoch, bumped on re-plan. Fences acceptance of a
    /// superseded generation's late events.
    pub generation: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

// ---------------------------------------------------------------------------
// Child + attempt + lease
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetChildRecord {
    pub schema_version: u32,
    pub fleet_id: String,
    /// `child_id == plan task_id` in v1.
    pub child_id: String,
    pub worker_kind: WorkerKind,
    pub status: ChildStatus,
    pub current_attempt_id: Option<String>,
    pub attempts_used: u32,
    /// Terminal outcome — an [`AcceptanceVerdict`], *not* merely "has a
    /// result". `None` until the child reaches a terminal state.
    pub outcome: Option<AcceptanceVerdict>,
    pub tokens_committed: u64,
    /// Task-ids that must be `Succeeded` before this child is `Ready`.
    /// Denormalized from `PlanTask.deps` (child_id == task_id) so the
    /// store's dep-resolution helper is self-contained.
    pub deps: Vec<String>,
    /// PR B — the pending mid-task escalation, set when a worker's attempt
    /// yields via the `escalate` tool (status → [`ChildStatus::Blocked`]). It
    /// is the worker's REQUEST for a wider grant; the keeper reads it (surfaced
    /// through [`crate::TaskView::pending_escalation`]) and decides. `Some`
    /// while a child is `Blocked`; cleared when the keeper's `goal_grant` (→
    /// `Ready`) or `goal_deny` (→ `Failed`) resolves it — so a re-run child
    /// never carries a stale request. `#[serde(default)]` is deliberate
    /// forward-compat, **NOT** a `SCHEMA_VERSION` bump: an old row written
    /// before escalation existed (no key) deserializes to `None` rather than
    /// dropping the row (mirrors how `grant`/`controller_workspace_root` were
    /// added).
    #[serde(default)]
    pub pending_escalation: Option<EscalationRequest>,
    pub generation: u64,
    pub updated_at_ms: u64,
}

/// A worker's mid-task request to WIDEN its [`crate::WorkerGrant`] (PR B). The
/// worker records this via the always-on `escalate` tool when it hits the edge
/// of its grant (a host/tool/fs it wasn't given) and yields its attempt; the
/// keeper reads it and decides.
///
/// `requested_grant` is **ADVISORY** — it is what the worker asked for, not what
/// it gets. The worker can NEVER self-widen: only the keeper's `goal_grant`
/// mutates [`crate::PlanTask::grant`], and it re-runs `grant.validate()` on the
/// grant IT chooses (which may be less than requested). This request only
/// informs the operator's decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationRequest {
    /// The grant the worker asked for (advisory — the keeper may grant less).
    pub requested_grant: WorkerGrant,
    /// Why the worker needs it (the model's own justification), for the
    /// operator's decision.
    pub reason: String,
}

/// The daemon-boot lease guarding a live attempt. `owner_epoch` is this
/// boot's id; a foreign epoch (or an expired `expires_at_ms`) is what
/// recovery reconciliation reclaims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub owner_epoch: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub schema_version: u32,
    /// The owning fleet. Denormalized onto the attempt so `mark_running`
    /// / `complete` can reach the child row (whose key is
    /// `{fleet_id}\0{child_id}`) from `(child_id, attempt_id)` alone.
    /// `#[serde(default)]`: forward-compat hygiene — a field added without
    /// a schema bump must deserialize on older rows (there is no on-disk
    /// data for this unmerged crate, so this is purely a discipline).
    #[serde(default)]
    pub fleet_id: String,
    pub child_id: String,
    /// Fresh UUID per attempt.
    pub attempt_id: String,
    /// v1.1 fix: immutable, stamped at launch. The Complete predicate
    /// needs it — attempt-id alone cannot fence a *same-generation*
    /// retry.
    pub generation: u64,
    pub status: AttemptStatus,
    pub lease: Lease,
    /// Tokens reserved on the fleet budget at launch. Complete settles
    /// `fleet.tokens_reserved -= reserved_tokens` in the same txn.
    pub reserved_tokens: u64,
    /// Verbatim replay payload (mirrors `DispatchRecord.final_result`);
    /// never recomputed.
    pub result_snapshot: Option<ChildResultSnapshot>,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
}

/// Self-contained snapshot of a completed attempt's result. Mirrors the
/// role of `DispatchRecord.final_result` but carries no `octos-agent`
/// types (this crate is dependency-isolated); the executor PR maps its
/// `TaskResult` into this shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChildResultSnapshot {
    pub output: String,
    pub success: bool,
    pub tokens_used: u64,
    pub files: Vec<String>,
    pub error: Option<String>,
}

/// A child's terminal verdict. `Accepted` carries the evidence that
/// satisfied acceptance; the two rejections carry a reason. This is the
/// `outcome` on a terminal [`FleetChildRecord`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcceptanceVerdict {
    Accepted { evidence: Vec<EvidenceRef> },
    Rejected { reason: String },
    Terminated { reason: String },
}

// ---------------------------------------------------------------------------
// Durable plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurablePlan {
    pub schema_version: u32,
    pub fleet_id: String,
    /// Bumped on every mutation; `mutate_plan` is a CAS on this value.
    pub revision: u64,
    pub objective: String,
    pub tasks: Vec<PlanTask>,
}

/// One task in the durable plan — the **spec** (author intent),
/// deliberately free of any execution state. There is exactly one home
/// per fact (PR-2 reconciliation): a task's *live state* is its child's
/// ([`FleetChildRecord::status`] + `current_attempt_id`), and its
/// *outcome + evidence* live with the child's
/// [`FleetChildRecord::outcome`] (an [`AcceptanceVerdict`], whose
/// `Accepted` variant carries the `EvidenceRef`s). Because
/// `child_id == task_id` there is no separate `assigned_child` pointer.
/// So `PlanTask` says *what* to do; `FleetChildRecord` says *how it is
/// going*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTask {
    pub task_id: String,
    pub title: String,
    pub detail: String,
    /// task_ids that must be `Succeeded` first. This is the author's
    /// declared spec; the store keeps a denormalized resolution copy on
    /// [`FleetChildRecord::deps`], re-synced by `replan`.
    pub deps: Vec<String>,
    pub acceptance: Vec<AcceptanceCriterion>,
    /// PR A — the operator-supplied capability grant the host builds this
    /// task's worker from (network / tools / filesystem). `#[serde(default)]`
    /// is deliberate forward-compat, **NOT** a `SCHEMA_VERSION` bump: an old
    /// row written before grants existed (no `grant` key) deserializes to
    /// [`WorkerGrant::minimal`] — byte-for-byte today's closed worker — rather
    /// than dropping the row. Least-privilege by default; the master expands it
    /// explicitly per task.
    #[serde(default)]
    pub grant: WorkerGrant,
}

/// An acceptance criterion: a description plus a checkable [`Verifier`].
/// "Done" is data + a verifier, never a bare boolean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
    pub verifier: Verifier,
}

/// How a criterion is checked. `Manual` is retained in the data model
/// for completeness; §6b of the spec drops it from the v1 *executor*
/// (no headless mapping) — that is an executor-layer policy (PR 3), not
/// a store-schema concern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verifier {
    Manual,
    FileExists { path: String },
    CommandExit { cmd: String, code: i32 },
    ValidatorRef { id: String },
}

/// A captured piece of evidence. `kind` is an open string (evidence is
/// produced by arbitrary verifiers), unlike the closed, kernel-emitted
/// [`DecisionKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub kind: String,
    pub locator: String,
    pub sha256: String,
    pub captured_at_ms: u64,
}

/// Append-only decision-log entry. `kind` is a closed set of
/// kernel-emitted decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionEntry {
    pub schema_version: u32,
    pub seq: u64,
    pub at_ms: u64,
    pub actor: String,
    pub kind: DecisionKind,
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionKind {
    Plan,
    Launch,
    Complete,
    Reconcile,
    Replan,
    Cancel,
    Note,
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// What a [`Finding`] asserts about its claim.
///
/// `RuledOut` is a first-class result, not an absence. A proven dead end and a
/// failed attempt are different facts — the first is knowledge that stops a
/// repeat, the second is noise — and [`ChildResultSnapshot`] cannot tell them
/// apart, because `success: false` is both. That is why a finding is its own
/// record rather than a field on an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindingStatus {
    /// Demonstrated, with evidence, under the stated `config`.
    Confirmed,
    /// Asserted from analysis before confirmation. A legitimate starting
    /// state, and one a dependency plan cannot express: a predicted wall can
    /// dissolve without ever being worked.
    Predicted,
    /// Demonstrated NOT to hold. The expensive kind, and most of why the
    /// record is worth keeping.
    RuledOut,
}

/// A durable unit of learning: one falsifiable claim, its evidence, and the
/// build state under which it holds.
///
/// Distinct from [`DecisionEntry`], which is a closed set of *kernel-emitted*
/// lifecycle events. A finding is a claim about the **problem domain**, made
/// by a worker, and the two must not share a table: "the kernel launched a
/// child" and "the second `eglCreateWindowSurface` returns `EGL_NO_SURFACE`"
/// are not the same kind of fact.
///
/// Findings are **append-only and never mutated**. Supersession is derived
/// from the `supersedes` edges of *later* findings (see [`superseded_ids`]),
/// so history stays reconstructable and no write ever contends with another —
/// which is what lets this table skip the CAS machinery the child/attempt
/// tables need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub schema_version: u32,
    /// `f-{seq}`. Assigned by the store; `supersedes` references it.
    pub id: String,
    pub seq: u64,
    pub fleet_id: String,
    /// The task whose exploration produced it, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,

    /// One falsifiable sentence. The test of a good claim: another worker can
    /// attempt to disprove it without first asking anyone a question.
    pub claim: String,
    pub status: FindingStatus,
    /// What the claim is *about* — the clustering key. Two paths whose recent
    /// findings cite one component is a mechanically computable hint that they
    /// share a root cause. That hint is most of what makes the synthesis
    /// available to a controller who never reads a transcript.
    pub component: String,

    /// Reused unchanged from the acceptance path: content-addressed, so a
    /// claim cannot drift from what was actually observed.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// Component → version under which the claim holds. A finding without
    /// this is not a weaker finding, it is an untrustworthy one: the next
    /// reader cannot tell whether it still applies. It is also the
    /// invalidation rule — when a version moves, findings scoped to the old
    /// one become STALE, which is a third state distinct from wrong.
    #[serde(default)]
    pub config: BTreeMap<String, String>,

    /// Findings this one overturns, by `id`. Validated to exist at append
    /// time: a dangling overturn edge is how a knowledge base rots into
    /// contradictions its readers learn to distrust.
    #[serde(default)]
    pub supersedes: Vec<String>,
    /// What it cost to learn. Feeds cost-against-yield per path, which is the
    /// input to abandoning one.
    #[serde(default)]
    pub cost_tokens: u64,

    pub by: String,
    pub at_ms: u64,

    // Additional fields from SQLite ledger schema (#1941)
    /// Finding kind (observation | hypothesis | diagnosis | constraint | experiment_result).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Lifecycle state (proposed | observed | reproduced | verified | refuted | superseded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    /// Confidence level (high | medium | low).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Review state (unreviewed | peer_reviewed | independently_reproduced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_state: Option<String>,

    // SQLite ledger fields (#1941)
    /// SQLite rowid (assigned on insert, never changes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rowid: Option<i64>,
    /// Derived from (JSON array of finding_ids).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
}

// ---------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------

/// A durable outbox event with a *real* claim/ack protocol (not
/// replay-only dedup). Enriched (§6b) so a consumer can route
/// `fleet_id → controller_session_key` and reclaim a crashed claim via
/// `claim_expires_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEvent {
    pub schema_version: u32,
    pub sequence: u64,
    pub event_id: String,
    pub fleet_id: String,
    pub child_id: Option<String>,
    pub attempt_id: Option<String>,
    pub kind: FleetEventKind,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub claimed_by: Option<String>,
    /// Unique per-claim token minted by `claim_next`. `ack` must present
    /// the matching `(claimed_by, claim_token)` — this fences a stale
    /// consumer whose lease already expired and was reclaimed (P1-3).
    #[serde(default)]
    pub claim_token: Option<String>,
    pub claim_expires_at: Option<u64>,
    pub acked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FleetEventKind {
    ChildLaunching,
    ChildRunning,
    ChildDone,
    FleetDrained,
}

// ---------------------------------------------------------------------------
// Version probe
// ---------------------------------------------------------------------------

/// Cheap probe that reads *only* `schema_version` from a row before the
/// full decode, so a higher-version row is dropped (`Ok(None)`) without
/// risking a shape-mismatch parse error.
#[derive(Deserialize)]
struct VersionProbe {
    schema_version: u32,
}

/// Decode a persisted JSON row, dropping it (`Ok(None)`) when its
/// `schema_version` exceeds [`SCHEMA_VERSION`] — a newer daemon's row is
/// opaque to this one, never mis-parsed. The version is probed *before*
/// the full decode so an incompatible shape cannot surface as a parse
/// error masquerading as corruption.
pub(crate) fn decode_row<T: serde::de::DeserializeOwned>(json: &str) -> eyre::Result<Option<T>> {
    let probe: VersionProbe = serde_json::from_str(json)?;
    if probe.schema_version > SCHEMA_VERSION {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(json)?))
}

/// Ids overturned by some later finding in `findings`.
///
/// Derived rather than stored: marking the old record would be a mutation,
/// and mutation is the only thing that would force this table to carry the
/// revision fencing the rest of the kernel needs.
pub fn superseded_ids(findings: &[Finding]) -> std::collections::BTreeSet<String> {
    findings
        .iter()
        .flat_map(|f| f.supersedes.iter().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_admits_is_inclusive_and_saturating() {
        let budget = FleetBudget {
            token_budget: 100,
            tokens_reserved: 40,
            tokens_committed: 30,
            hard: false,
        };
        // 40 + 30 + 30 == 100 -> inclusive admit.
        assert!(budget.admits(30));
        // 40 + 30 + 31 == 101 -> reject.
        assert!(!budget.admits(31));
        // Saturating: an absurd projection cannot wrap to admit.
        assert!(!budget.admits(u64::MAX));
    }

    #[test]
    fn budget_admits_rejects_on_overflow_instead_of_saturating() {
        // P2-5: with saturating math `MAX + MAX + 1` clamps to MAX and
        // would spuriously admit against a MAX budget. Checked math must
        // reject the overflow.
        let budget = FleetBudget {
            token_budget: u64::MAX,
            tokens_reserved: u64::MAX,
            tokens_committed: 0,
            hard: false,
        };
        assert!(!budget.admits(1));
    }

    #[test]
    fn records_round_trip_through_json() {
        let verdict = AcceptanceVerdict::Accepted {
            evidence: vec![EvidenceRef {
                kind: "file".into(),
                locator: "out.txt".into(),
                sha256: "deadbeef".into(),
                captured_at_ms: 7,
            }],
        };
        let json = serde_json::to_string(&verdict).unwrap();
        let back: AcceptanceVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(verdict, back);
    }

    #[test]
    fn child_status_terminal_classification() {
        assert!(ChildStatus::Succeeded.is_terminal());
        assert!(ChildStatus::Failed.is_terminal());
        assert!(ChildStatus::Cancelled.is_terminal());
        assert!(!ChildStatus::Ready.is_terminal());
        assert!(!ChildStatus::Running.is_terminal());
        // PR B: Blocked is a pause awaiting an operator decision — NOT terminal,
        // so it holds `is_complete` open (the goal stays active).
        assert!(!ChildStatus::Blocked.is_terminal());
    }

    #[test]
    fn child_record_pending_escalation_round_trips_and_old_row_defaults_none() {
        use crate::grant::{NetworkGrant, WorkerGrant};

        // A blocked child carrying a pending escalation round-trips verbatim.
        let rec = FleetChildRecord {
            schema_version: SCHEMA_VERSION,
            fleet_id: "f1".into(),
            child_id: "t1".into(),
            worker_kind: WorkerKind::StatelessTask,
            status: ChildStatus::Blocked,
            current_attempt_id: None,
            attempts_used: 1,
            outcome: None,
            tokens_committed: 80,
            deps: vec![],
            pending_escalation: Some(EscalationRequest {
                requested_grant: WorkerGrant {
                    network: NetworkGrant::Hosts(vec!["example.com".into()]),
                    tools: vec!["read_file".into(), "web_fetch".into()],
                    ..WorkerGrant::minimal()
                },
                reason: "needs example.com to fetch the report".into(),
            }),
            generation: 0,
            updated_at_ms: 5,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: FleetChildRecord = decode_row(&json).unwrap().unwrap();
        assert_eq!(back, rec, "the pending escalation round-trips through JSON");

        // An OLD row written before escalation existed (no `pending_escalation`
        // key) must still decode — `#[serde(default)]` fills `None`, NOT fail.
        // This is why the field is NOT a `SCHEMA_VERSION` bump.
        let old_json = r#"{
            "schema_version": 2,
            "fleet_id": "f1",
            "child_id": "t1",
            "worker_kind": "StatelessTask",
            "status": "Ready",
            "current_attempt_id": null,
            "attempts_used": 0,
            "outcome": null,
            "tokens_committed": 0,
            "deps": [],
            "generation": 0,
            "updated_at_ms": 0
        }"#;
        let legacy: FleetChildRecord = decode_row(old_json)
            .expect("decode old row")
            .expect("row kept, not dropped");
        assert_eq!(
            legacy.pending_escalation, None,
            "a child with no escalation field loads as None"
        );
    }

    #[test]
    fn plan_task_grant_persists_and_restores() {
        use crate::grant::{FsGrant, NetworkGrant, WorkerGrant};

        // A granted task round-trips its grant verbatim through JSON (the shape
        // the FleetKernelStore persists).
        let task = PlanTask {
            task_id: "t1".into(),
            title: "fetch".into(),
            detail: "grab the report".into(),
            deps: vec![],
            acceptance: vec![],
            grant: WorkerGrant {
                network: NetworkGrant::Hosts(vec!["example.com".into()]),
                tools: vec!["read_file".into(), "write_file".into(), "web_fetch".into()],
                fs: FsGrant::Host,
            },
        };
        // PlanTask is nested inside DurablePlan (which carries schema_version);
        // it has no version field of its own, so it round-trips via plain serde.
        let json = serde_json::to_string(&task).unwrap();
        let back: PlanTask = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.grant, task.grant,
            "grant round-trips through the store"
        );

        // An OLD row written before grants existed (no `grant` key) must still
        // decode — `#[serde(default)]` fills `WorkerGrant::minimal()` (today's
        // closed worker), NOT fail. This is why grant is not a SCHEMA_VERSION
        // bump.
        let old_json = r#"{
            "task_id": "t0",
            "title": "legacy",
            "detail": "",
            "deps": [],
            "acceptance": []
        }"#;
        let legacy: PlanTask = serde_json::from_str(old_json).expect("decode old row");
        assert_eq!(
            legacy.grant,
            WorkerGrant::minimal(),
            "a task with no grant loads as the least-privilege closed worker",
        );
    }

    #[test]
    fn fleet_record_without_workspace_root_deserializes_to_none() {
        // PR 4b schema back-compat: an OLD row written before
        // `controller_workspace_root` existed (no such key) must still decode —
        // `#[serde(default)]` fills `None` rather than erroring. This is why the
        // field is NOT a `SCHEMA_VERSION` bump: `decode_row` keeps the row
        // (version unchanged at 2) and serde defaults the missing field.
        let old_json = r#"{
            "schema_version": 2,
            "fleet_id": "f1",
            "controller_session_key": "api:keeper-1",
            "profile_id": "default",
            "budget": {
                "token_budget": 1000,
                "tokens_reserved": 0,
                "tokens_committed": 0,
                "hard": false
            },
            "status": "Active",
            "generation": 0,
            "created_at_ms": 5,
            "updated_at_ms": 5
        }"#;
        let rec: FleetRecord = decode_row(old_json)
            .expect("decode old row")
            .expect("row kept, not dropped");
        assert_eq!(
            rec.controller_workspace_root, None,
            "a missing field defaults to None"
        );
        assert_eq!(
            rec.controller_workspace_has_runtime_hint, None,
            "legacy rows have unknown workspace provenance and must fail safe"
        );

        // And a NEW row with the field round-trips it verbatim.
        let with_root = FleetRecord {
            controller_workspace_root: Some("/repos/app".to_owned()),
            controller_workspace_has_runtime_hint: Some(false),
            ..rec
        };
        let json = serde_json::to_string(&with_root).unwrap();
        let back: FleetRecord = decode_row(&json).unwrap().unwrap();
        assert_eq!(
            back.controller_workspace_root,
            Some("/repos/app".to_owned())
        );
        assert_eq!(
            back.controller_workspace_has_runtime_hint,
            Some(false),
            "derived-workspace provenance round-trips with the root"
        );
    }
}
