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

/// Schema version stamped on every persisted record. Bumping this
/// invalidates prior rows the same way the swarm dispatch ledger does:
/// a load that sees a *higher* version returns `Ok(None)`.
pub const SCHEMA_VERSION: u32 = 1;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChildStatus {
    Planned,
    Ready,
    Launching,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl ChildStatus {
    /// A child in a terminal state cannot launch or transition further
    /// (other than an explicit cancel, which is itself terminal).
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

/// Plan-task state (the durable plan's view; distinct from the child's
/// runtime [`ChildStatus`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Pending,
    Ready,
    Assigned,
    Running,
    Blocked { reason: String },
    Accepted,
    Rejected { reason: String },
    Cancelled,
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
    pub generation: u64,
    pub updated_at_ms: u64,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTask {
    pub task_id: String,
    pub title: String,
    pub detail: String,
    /// task_ids that must be `Accepted`/`Succeeded` first.
    pub deps: Vec<String>,
    pub state: TaskState,
    pub acceptance: Vec<AcceptanceCriterion>,
    pub evidence: Vec<EvidenceRef>,
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
    }
}
