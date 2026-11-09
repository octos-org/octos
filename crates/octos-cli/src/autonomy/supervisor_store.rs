//! Durable supervisor state store for supervised agent groups.
//!
//! The store is intentionally small: an append-only JSONL event ledger plus a
//! snapshot file. It is standalone so the runtime can wire it in later without
//! forcing API handlers to depend on supervisor internals.
//!
//! Scaling model (#1974): appends assign sequences from an in-memory cursor
//! that is revalidated cheaply against the ledger under the cross-process file
//! lock (O(tail) instead of a full JSONL re-parse per append). Every
//! [`SNAPSHOT_EVERY_APPENDS`] ledger rows, the appending process writes a
//! durable snapshot and rotates the applied rows to a single `.jsonl.old`
//! forensics generation, so `load_state` is snapshot + a short tail replay.
//! Legacy dirs (JSONL only, no snapshot) load unchanged forever.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const EVENTS_FILE_NAME: &str = "supervisor-events.jsonl";
/// Single rotated ledger generation kept after each compaction, for
/// forensics only — it is never replayed. BACK/DOWN-COMPAT: a downgraded,
/// snapshot-UNAWARE binary (pre-#1974 builds) ignores the snapshot file and
/// replays only the live `.jsonl` tail, i.e. it sees a truncated view of any
/// store that has compacted; the most recent rotated prefix stays here for
/// manual recovery. Binaries that DO know snapshots refuse a snapshot with a
/// newer `schema_version` instead of misreading it (see `load_snapshot`).
const EVENTS_ROTATED_FILE_NAME: &str = "supervisor-events.jsonl.old";
const EVENTS_LOCK_FILE_NAME: &str = "supervisor-events.lock";
const SNAPSHOT_FILE_NAME: &str = "supervisor-snapshot.json";
const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const APPEND_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const APPEND_LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);
const AUTO_GROUP_TERMINAL_MESSAGE: &str = "all supervised children reached a terminal state";

/// Auto-snapshot/compaction cadence: once the live JSONL ledger holds this
/// many rows, the next `append_event` writes a snapshot and rotates the
/// applied rows away. 512 keeps the boot replay tail small (a few hundred KB
/// of JSON at typical event sizes, parsed in milliseconds) while amortizing
/// the full-state serialize + fsync cost over hundreds of appends. The trigger
/// counts rows in the ledger — not per-process appends — so several writers
/// sharing one ledger still compact once the tail crosses the threshold, and
/// a fat legacy ledger is healed by its first post-upgrade append.
pub const SNAPSHOT_EVERY_APPENDS: u64 = 512;

/// Initial window for locating the final ledger line without reading the
/// whole file; doubled until a full line is covered, so rows larger than this
/// still resolve.
const TAIL_PROBE_BYTES: u64 = 8 * 1024;

pub type SupervisorMetadata = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    // Precise non-running goal states (codex MED). Mapping a paused goal to
    // `Cancelled` or a blocked goal to `Failed` misleads a roster that renders
    // GroupStatus — a paused goal is not cancelled and a budget-capped goal is
    // not a hard failure. These variants keep the roster honest. They are only
    // ever produced by `group_status_for_goal`; no exhaustive `match` on
    // `GroupStatus` exists, and GroupStatus never crosses the wire protocol
    // (the roster's "orchestrating" indicator derives from live orchestration
    // counts, and the goal's precise status string is also carried in the
    // group metadata), so adding them is contained to this crate.
    Paused,
    Blocked,
    BudgetLimited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    Starting,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalKind {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationStatus {
    Queued,
    Started,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisedGroupRecord {
    pub group_id: String,
    #[serde(default)]
    pub supervisor_id: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub parent_turn_id: Option<String>,
    #[serde(default)]
    pub objective: Option<String>,
    pub status: GroupStatus,
    #[serde(default)]
    pub child_ids: Vec<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub terminal: Option<TerminalState>,
    #[serde(default)]
    pub metadata: SupervisorMetadata,
}

impl SupervisedGroupRecord {
    pub fn new(group_id: impl Into<String>, created_at_ms: u64) -> Self {
        Self {
            group_id: group_id.into(),
            supervisor_id: None,
            parent_session_id: None,
            parent_turn_id: None,
            objective: None,
            status: GroupStatus::Running,
            child_ids: Vec::new(),
            created_at_ms,
            updated_at_ms: created_at_ms,
            terminal: None,
            metadata: SupervisorMetadata::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildAgentRecord {
    pub group_id: String,
    pub child_id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub workspace_path: Option<String>,
    pub status: ChildStatus,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub last_heartbeat: Option<HeartbeatPing>,
    #[serde(default)]
    pub terminal: Option<TerminalState>,
    #[serde(default)]
    pub metadata: SupervisorMetadata,
}

impl ChildAgentRecord {
    pub fn new(
        group_id: impl Into<String>,
        child_id: impl Into<String>,
        started_at_ms: u64,
    ) -> Self {
        Self {
            group_id: group_id.into(),
            child_id: child_id.into(),
            label: None,
            profile_id: None,
            model: None,
            task: None,
            workspace_path: None,
            status: ChildStatus::Running,
            started_at_ms,
            updated_at_ms: started_at_ms,
            last_heartbeat: None,
            terminal: None,
            metadata: SupervisorMetadata::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatPing {
    pub group_id: String,
    pub child_id: String,
    #[serde(default)]
    pub ping_id: Option<String>,
    pub observed_at_ms: u64,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub progress_percent: Option<u8>,
    #[serde(default)]
    pub metadata: SupervisorMetadata,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalState {
    pub kind: TerminalKind,
    pub finished_at_ms: u64,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub metadata: SupervisorMetadata,
}

impl TerminalState {
    pub fn completed(finished_at_ms: u64, message: Option<String>) -> Self {
        Self {
            kind: TerminalKind::Completed,
            finished_at_ms,
            exit_code: Some(0),
            reason: None,
            message,
            metadata: SupervisorMetadata::new(),
        }
    }

    pub fn failed(finished_at_ms: u64, exit_code: Option<i32>, reason: Option<String>) -> Self {
        Self {
            kind: TerminalKind::Failed,
            finished_at_ms,
            exit_code,
            reason,
            message: None,
            metadata: SupervisorMetadata::new(),
        }
    }

    pub fn cancelled(finished_at_ms: u64, reason: Option<String>) -> Self {
        Self {
            kind: TerminalKind::Cancelled,
            finished_at_ms,
            exit_code: None,
            reason,
            message: None,
            metadata: SupervisorMetadata::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub group_id: String,
    #[serde(default)]
    pub child_id: Option<String>,
    pub artifact_id: String,
    pub kind: String,
    pub path: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub version: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub metadata: SupervisorMetadata,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingContinuationRecord {
    pub group_id: String,
    pub continuation_id: String,
    #[serde(default)]
    pub child_id: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    pub status: ContinuationStatus,
    pub queued_at_ms: u64,
    #[serde(default)]
    pub started_at_ms: Option<u64>,
    #[serde(default)]
    pub completed_at_ms: Option<u64>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub metadata: SupervisorMetadata,
}

/// Outcome of a retry that must never supersede a completed continuation.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ContinuationQueueOutcome {
    Written(SupervisorEventLedgerRow),
    AlreadyCompleted(PendingContinuationRecord),
}

/// Optional cohort epoch committed with a complete child admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CohortEpochAdmission {
    pub cwd_hash: u64,
    pub epoch: u64,
}

// Several variants carry their full record by value (group/child/artifact/
// continuation) because events are persisted and replayed as self-contained
// payloads; boxing would complicate serde round-trips for no hot-path win.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum SupervisorEvent {
    GroupRegistered {
        group: SupervisedGroupRecord,
    },
    GroupTerminal {
        group_id: String,
        terminal: TerminalState,
    },
    ChildStarted {
        child: ChildAgentRecord,
    },
    /// A checked admission or update. The complete child and its optional
    /// cohort binding share one ledger row, including terminal-first mirrors.
    AgentAdmitted {
        group: SupervisedGroupRecord,
        child: ChildAgentRecord,
        #[serde(default)]
        cohort: Option<CohortEpochAdmission>,
    },
    Heartbeat {
        ping: HeartbeatPing,
    },
    ChildTerminal {
        group_id: String,
        child_id: String,
        terminal: TerminalState,
    },
    ArtifactUpdated {
        artifact: ArtifactRecord,
    },
    ContinuationQueued {
        continuation: PendingContinuationRecord,
    },
    ContinuationStarted {
        group_id: String,
        continuation_id: String,
        started_at_ms: u64,
        /// #26 (round-4, #18 B4) — the revision (persisted `attempt`) of the
        /// queued payload THIS start belongs to. `#[serde(default)]` keeps
        /// pre-#26 events readable: they deserialize at 0 and the apply-side
        /// revision check treats a 0 as "no revision carried" (legacy
        /// behavior — always applied).
        #[serde(default)]
        attempt: u32,
    },
    ContinuationCompleted {
        group_id: String,
        continuation_id: String,
        completed_at_ms: u64,
        #[serde(default)]
        result: Option<String>,
        /// #26 (round-4, #18 B4) — same as `ContinuationStarted::attempt`:
        /// the queued revision this completion resolves. A Completed for an
        /// OLD attempt must never tombstone a NEWER revision that has not
        /// executed yet (crash window: attempt-1 turn finishes after the
        /// attempt-2 correction was already durably queued).
        #[serde(default)]
        attempt: u32,
    },
    /// #15b — durable join-epoch bump marker. `upsert_agent` bumps a group's
    /// join epoch IN MEMORY when a new agent is admitted into an
    /// already-joined group; without this record a crash before the bumped
    /// epoch's scatter persists would restart at the last persisted epoch and
    /// the reconcile pass could never derive the lost epoch's join key.
    /// `apply_event` upserts the group's cohort-scoped `join_epoch` metadata
    /// (max wins).
    ///
    /// #19 (round-4 B1) — `cwd_hash` carries the admitted agent's workspace
    /// cohort (the same `DefaultHasher`-over-`Option<String>` value the join
    /// key embeds): a bump under one workspace must not lift another
    /// workspace's join epoch. `#[serde(default)]` keeps pre-#19 events
    /// readable — they deserialize with 0 and map onto the NONE-workspace
    /// cohort; pre-#19 stores only ever persisted the None-cwd cohort in
    /// practice (every existing test fixture upserts with `cwd: None`), so
    /// this fallback is exact there and best-effort elsewhere.
    GroupEpochBumped {
        group_id: String,
        new_epoch: u64,
        observed_at_ms: u64,
        #[serde(default)]
        cwd_hash: u64,
    },
    /// #20 (round-4 B2) — ATOMIC cohort admission. One durable event binds
    /// the cohort admission, the admitted child's identity, AND the bumped
    /// join epoch, replacing the two-step "bump in memory, then best-effort
    /// `GroupEpochBumped`, then insert + persist the child" sequence whose
    /// crash windows could (a) lose a join (marker failed, child durable) or
    /// (b) fabricate a phantom join (marker durable, child lost). The event
    /// id embeds the child id, so replaying the SAME admission is idempotent.
    ///
    /// `apply_event` appends `child_id:new_epoch` to the group's
    /// `admissions#{cwd_hash}` metadata (comma list, append-if-absent) and
    /// max-upserts the cohort-scoped `join_epoch#{cwd_hash}` marker — so the
    /// restore side reads ONE metadata surface. The seed pass only TRUSTS an
    /// admission whose child exists in `children` (durable child record),
    /// which closes the phantom-join window.
    CohortAdmission {
        group_id: String,
        #[serde(default)]
        cwd_hash: u64,
        child_id: String,
        new_epoch: u64,
        observed_at_ms: u64,
    },
}

/// One folded-extra tombstone in a coalesced terminal-continuation fold
/// (#1707 round 4). `result` names the carrier the extra folded into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedTombstoneEntry {
    pub group_id: String,
    pub continuation_id: String,
    pub completed_at_ms: u64,
    pub result: String,
    /// #26 (round-4, #18 B4) — the revision the tombstone resolves. 0 keeps
    /// the legacy event id / unconditional-apply shape (folded extras whose
    /// durable attempt is unknown or 0); a positive value carries the same
    /// revision-match rule as single-record completions.
    pub attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisorEventLedgerRow {
    pub event_id: String,
    pub sequence: u64,
    pub recorded_at_ms: u64,
    pub event: SupervisorEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisorSnapshot {
    pub schema_version: u32,
    pub written_at_ms: u64,
    pub last_sequence: u64,
    pub state: SupervisorState,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SupervisorState {
    #[serde(default)]
    pub groups: HashMap<String, SupervisedGroupRecord>,
    #[serde(default)]
    pub children: HashMap<String, ChildAgentRecord>,
    #[serde(default)]
    pub artifacts: HashMap<String, ArtifactRecord>,
    #[serde(default)]
    pub continuations: HashMap<String, PendingContinuationRecord>,
    #[serde(default)]
    pub applied_event_ids: HashSet<String>,
    #[serde(default)]
    pub last_sequence: u64,
}

impl SupervisorState {
    pub fn apply_ledger_row(&mut self, row: &SupervisorEventLedgerRow) {
        if self.accept_ledger_row(row) {
            self.apply_event(&row.event, row.recorded_at_ms);
        }
    }

    // Both full replay and the continuation projection deduplicate ALL event
    // kinds globally, after advancing the sequence even for a duplicate.
    fn accept_ledger_row(&mut self, row: &SupervisorEventLedgerRow) -> bool {
        self.last_sequence = self.last_sequence.max(row.sequence);
        row.event_id.is_empty() || self.applied_event_ids.insert(row.event_id.clone())
    }

    pub fn apply_event(&mut self, event: &SupervisorEvent, recorded_at_ms: u64) {
        match event {
            SupervisorEvent::GroupRegistered { group } => self.upsert_group(group.clone()),
            SupervisorEvent::GroupTerminal { group_id, terminal } => {
                let group = self.ensure_group(group_id, recorded_at_ms);
                if should_replace_terminal(&group.terminal, terminal) {
                    group.status = group_status_for_terminal(&terminal.kind);
                    group.updated_at_ms = group.updated_at_ms.max(terminal.finished_at_ms);
                    group.terminal = Some(terminal.clone());
                }
            }
            SupervisorEvent::ChildStarted { child } => self.upsert_child(child.clone()),
            SupervisorEvent::AgentAdmitted {
                group,
                child,
                cohort,
            } => {
                self.apply_agent_admitted(group.clone(), child.clone(), cohort.as_ref());
            }
            SupervisorEvent::Heartbeat { ping } => self.apply_heartbeat(ping.clone()),
            SupervisorEvent::ChildTerminal {
                group_id,
                child_id,
                terminal,
            } => self.apply_child_terminal(group_id, child_id, terminal.clone(), recorded_at_ms),
            SupervisorEvent::ArtifactUpdated { artifact } => self.upsert_artifact(artifact.clone()),
            SupervisorEvent::ContinuationQueued { continuation } => {
                self.upsert_continuation(continuation.clone())
            }
            SupervisorEvent::ContinuationStarted {
                group_id,
                continuation_id,
                started_at_ms,
                attempt,
            } => {
                self.apply_continuation_started(group_id, continuation_id, *started_at_ms, *attempt)
            }
            SupervisorEvent::ContinuationCompleted {
                group_id,
                continuation_id,
                completed_at_ms,
                result,
                attempt,
            } => self.apply_continuation_completed(
                group_id,
                continuation_id,
                *completed_at_ms,
                result.clone(),
                *attempt,
            ),
            SupervisorEvent::GroupEpochBumped {
                group_id,
                new_epoch,
                observed_at_ms,
                cwd_hash,
            } => {
                // #15b — durable epoch bump: max-upsert the group's
                // `join_epoch` metadata so a restart restores the HIGHEST
                // admitted epoch, not just the highest PERSISTED scatter.
                // #19 (round-4 B1) — the metadata key is cohort-scoped
                // (`join_epoch#{cwd_hash}`): each workspace cohort restores
                // its OWN highest admitted epoch. Pre-#19 events carry
                // `cwd_hash: 0` via `#[serde(default)]` and keep landing on
                // the NONE-workspace cohort key.
                let group = self.ensure_group(group_id, *observed_at_ms);
                let metadata_key = format!("join_epoch#{cwd_hash}");
                let existing = group
                    .metadata
                    .get(&metadata_key)
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                if *new_epoch > existing {
                    group
                        .metadata
                        .insert(metadata_key, serde_json::Value::from(*new_epoch));
                }
                group.updated_at_ms = group.updated_at_ms.max(*observed_at_ms);
            }
            SupervisorEvent::CohortAdmission {
                group_id,
                cwd_hash,
                child_id,
                new_epoch,
                observed_at_ms,
            } => {
                // #20 (round-4 B2) — record the admission in the cohort's
                // `admissions#{cwd_hash}` metadata as a `child_id:new_epoch`
                // comma list (append-if-absent by CHILD, so a replayed
                // admission is a no-op). This is deliberately the ONLY
                // metadata the event writes: the seed pass derives the cohort
                // epoch from this list under the child-exists gate (an
                // admission whose child record is not durable does NOT lift
                // the epoch), whereas the legacy `join_epoch#{cwd_hash}`
                // marker — written by the pre-#20 `GroupEpochBumped` arm — is
                // read ungated for backward compatibility. Writing both from
                // THIS arm would re-open the phantom-join window.
                let group = self.ensure_group(group_id, *observed_at_ms);
                let admissions_key = format!("admissions#{cwd_hash}");
                let entry = format!("{child_id}:{new_epoch}");
                let mut list: Vec<String> = group
                    .metadata
                    .get(&admissions_key)
                    .and_then(|value| value.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.split(',').map(str::to_owned).collect())
                    .unwrap_or_default();
                if !list
                    .iter()
                    .any(|e| e.split(':').next() == Some(child_id.as_str()))
                {
                    list.push(entry);
                    group
                        .metadata
                        .insert(admissions_key, serde_json::Value::from(list.join(",")));
                }
                group.updated_at_ms = group.updated_at_ms.max(*observed_at_ms);
            }
        }
    }

    fn apply_agent_admitted(
        &mut self,
        mut group: SupervisedGroupRecord,
        mut child: ChildAgentRecord,
        cohort: Option<&CohortEpochAdmission>,
    ) {
        let group_id = group.group_id.clone();
        let observed_at_ms = group.updated_at_ms.max(child.updated_at_ms);
        // Admission callers build a fresh group description. Preserve prior
        // registration data and admission/epoch history when merging it.
        // Remember supplied child ids after merging so a newly seen child
        // still reopens an automatically completed group.
        let declared_children = std::mem::take(&mut group.child_ids);
        if let Some(existing) = self.groups.get(&group_id) {
            group.created_at_ms = group.created_at_ms.min(existing.created_at_ms);
            group.child_ids = existing.child_ids.clone();
            group.supervisor_id = group
                .supervisor_id
                .or_else(|| existing.supervisor_id.clone());
            group.parent_session_id = group
                .parent_session_id
                .or_else(|| existing.parent_session_id.clone());
            group.parent_turn_id = group
                .parent_turn_id
                .or_else(|| existing.parent_turn_id.clone());
            group.objective = group.objective.or_else(|| existing.objective.clone());
            // Existing group metadata may include replay-derived state. A
            // child update must not replace that with an older group view.
            group.metadata.extend(existing.metadata.clone());
            if group.terminal.is_none() {
                group.terminal = existing.terminal.clone();
                group.status = existing.status.clone();
            }
        }
        self.upsert_group(group);
        for child_id in declared_children {
            self.remember_child(&group_id, &child_id, observed_at_ms);
        }

        if let Some(existing) = self.children.get(&child_key(&group_id, &child.child_id)) {
            // A durable update can replace the last current observation of
            // an older workspace. Retain only its small provenance record so
            // restore can still interpret legacy cohort hashes exactly.
            let scope_changed = child.metadata.get("workspace_scope").is_some_and(|scope| {
                existing
                    .metadata
                    .get("workspace_scope")
                    .unwrap_or(&serde_json::Value::Null)
                    != scope
            });
            let workspace_changed =
                existing.workspace_path != child.workspace_path || scope_changed;
            let mut workspace_history = existing
                .metadata
                .get("workspace_history")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            if workspace_changed {
                let observation = serde_json::json!({
                    "workspace_path": existing.workspace_path,
                    "workspace_scope": existing.metadata.get("workspace_scope"),
                    "backend_kind": existing.metadata.get("backend_kind"),
                    "role": existing.metadata.get("role"),
                    "nickname": existing.metadata.get("nickname"),
                    "label": existing.label,
                });
                if !workspace_history.contains(&observation) {
                    workspace_history.push(observation);
                }
            }
            // Status/workspace and explicitly supplied metadata belong to
            // the candidate. Observations accumulated by other events must
            // survive a candidate that does not carry them.
            child.started_at_ms = child.started_at_ms.min(existing.started_at_ms);
            if child.last_heartbeat.is_none() {
                child.last_heartbeat = existing.last_heartbeat.clone();
            }
            let mut supplied_metadata = std::mem::take(&mut child.metadata);
            // History is derived from actual prior records, never replaced
            // by the candidate's potentially incomplete view of the child.
            supplied_metadata.remove("workspace_history");
            child.metadata = existing.metadata.clone();
            child.metadata.extend(supplied_metadata);
            if workspace_changed {
                child.metadata.insert(
                    "workspace_history".into(),
                    serde_json::Value::Array(workspace_history),
                );
            }
        }
        let child_id = child.child_id.clone();
        self.upsert_child(child);
        if let Some(cohort) = cohort {
            let group = self.ensure_group(&group_id, observed_at_ms);
            let key = format!("admissions#{}", cohort.cwd_hash);
            let entry = format!("{child_id}:{}", cohort.epoch);
            let mut admissions: Vec<String> = group
                .metadata
                .get(&key)
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(|value| value.split(',').map(str::to_owned).collect())
                .unwrap_or_default();
            if !admissions.contains(&entry) {
                admissions.push(entry);
                group
                    .metadata
                    .insert(key, serde_json::Value::from(admissions.join(",")));
            }
            group.updated_at_ms = group.updated_at_ms.max(observed_at_ms);
        }
        self.recompute_group_terminal(&group_id);
    }

    fn upsert_group(&mut self, group: SupervisedGroupRecord) {
        match self.groups.get_mut(&group.group_id) {
            Some(existing) => {
                let existing_children = existing.child_ids.clone();
                if group.updated_at_ms >= existing.updated_at_ms {
                    *existing = group;
                }
                for child_id in existing_children {
                    push_unique(&mut existing.child_ids, child_id);
                }
            }
            None => {
                self.groups.insert(group.group_id.clone(), group);
            }
        }
    }

    fn upsert_child(&mut self, mut child: ChildAgentRecord) {
        let key = child_key(&child.group_id, &child.child_id);
        self.ensure_group(&child.group_id, child.started_at_ms);
        self.remember_child(&child.group_id, &child.child_id, child.started_at_ms);
        match self.children.get_mut(&key) {
            Some(existing) => {
                if existing.terminal.is_some() && child.terminal.is_none() {
                    child.terminal = existing.terminal.clone();
                    child.status = existing.status.clone();
                }
                if child.updated_at_ms >= existing.updated_at_ms {
                    *existing = child;
                }
            }
            None => {
                self.children.insert(key, child);
            }
        }
    }

    fn apply_heartbeat(&mut self, ping: HeartbeatPing) {
        self.ensure_group(&ping.group_id, ping.observed_at_ms);
        self.remember_child(&ping.group_id, &ping.child_id, ping.observed_at_ms);
        let key = child_key(&ping.group_id, &ping.child_id);
        let child = self.children.entry(key).or_insert_with(|| {
            ChildAgentRecord::new(&ping.group_id, &ping.child_id, ping.observed_at_ms)
        });
        if child
            .last_heartbeat
            .as_ref()
            .is_none_or(|existing| ping.observed_at_ms >= existing.observed_at_ms)
        {
            child.updated_at_ms = child.updated_at_ms.max(ping.observed_at_ms);
            child.last_heartbeat = Some(ping);
            if child.terminal.is_none() {
                child.status = ChildStatus::Running;
            }
        }
    }

    fn apply_child_terminal(
        &mut self,
        group_id: &str,
        child_id: &str,
        terminal: TerminalState,
        recorded_at_ms: u64,
    ) {
        self.ensure_group(group_id, recorded_at_ms);
        self.remember_child(group_id, child_id, recorded_at_ms);
        let key = child_key(group_id, child_id);
        let child = self
            .children
            .entry(key)
            .or_insert_with(|| ChildAgentRecord::new(group_id, child_id, recorded_at_ms));
        if should_replace_terminal(&child.terminal, &terminal) {
            child.updated_at_ms = child.updated_at_ms.max(terminal.finished_at_ms);
            child.status = child_status_for_terminal(&terminal.kind);
            child.terminal = Some(terminal);
        }
        self.recompute_group_terminal(group_id);
    }

    fn upsert_artifact(&mut self, artifact: ArtifactRecord) {
        self.ensure_group(&artifact.group_id, artifact.updated_at_ms);
        let key = artifact_key(&artifact.group_id, &artifact.artifact_id);
        match self.artifacts.get_mut(&key) {
            Some(existing) => {
                if artifact.version > existing.version
                    || (artifact.version == existing.version
                        && artifact.updated_at_ms >= existing.updated_at_ms)
                {
                    *existing = artifact;
                }
            }
            None => {
                self.artifacts.insert(key, artifact);
            }
        }
    }

    fn upsert_continuation(&mut self, continuation: PendingContinuationRecord) {
        self.ensure_group(&continuation.group_id, continuation.queued_at_ms);
        let key = continuation_key(&continuation.group_id, &continuation.continuation_id);
        match self.continuations.get_mut(&key) {
            Some(existing) => {
                // #1707 round 3 (codex Blocker 3): `attempt` is the REVISION
                // of a queued payload for this continuation id. A status
                // correction (`failed` → `completed` on the same identity
                // dedupe key) persists a fresh `Queued` record with a
                // strictly higher attempt — and the status-rank gate alone
                // (`Queued 0 < Completed 2`) would silently DROP it behind a
                // delivered item's tombstone, resurrecting the OLD payload on
                // restart replay. So a strictly-higher attempt is
                // rank-eligible regardless of status; a same-or-lower attempt
                // can never downgrade a higher-rank record (an attempt-1
                // re-persist behind the tombstone stays dropped).
                let attempt_eligible = continuation.attempt > existing.attempt;
                if attempt_eligible
                    || continuation_rank(&continuation.status)
                        >= continuation_rank(&existing.status)
                {
                    *existing = merge_continuation(existing.clone(), continuation);
                }
            }
            None => {
                self.continuations.insert(key, continuation);
            }
        }
    }

    /// #26 (round-4, #18 B4) — revision-matched terminal transitions. A
    /// Started/Completed for an OLD attempt must not touch a record that
    /// now holds a NEWER queued revision: the attempt-1 turn may finish
    /// (and write its Completed) AFTER a status correction durably queued
    /// attempt 2; applying that Completed would tombstone the not-yet-run
    /// correction, and after a crash neither replay nor the seeded gate
    /// would surface it again. Rule: an incoming lifecycle event carries
    /// the attempt of the payload it resolved; it applies only when the
    /// record's current attempt is EQUAL or LOWER (equal = the normal
    /// same-revision path; lower = the record is an older legacy row the
    /// event still describes). A STRICTLY HIGHER record attempt means a
    /// newer revision superseded this event's payload — the event is
    /// ignored (warn) so the newer revision stays Queued for redelivery.
    /// `incoming == 0` is the legacy shape (pre-#26 events deserialize
    /// without an attempt): applied unconditionally, preserving the
    /// historical behavior of old ledgers.
    fn continuation_lifecycle_attempt_matches(
        &self,
        key: &str,
        incoming_attempt: u32,
        event_kind: &str,
        continuation_id: &str,
    ) -> bool {
        let Some(record) = self.continuations.get(key) else {
            return true;
        };
        if incoming_attempt == 0 {
            return true;
        }
        if record.attempt > incoming_attempt {
            tracing::warn!(
                event = event_kind,
                continuation_id,
                record_attempt = record.attempt,
                event_attempt = incoming_attempt,
                "ignoring lifecycle event for a superseded attempt; the newer \
                 queued revision stays pending"
            );
            return false;
        }
        true
    }

    fn apply_continuation_started(
        &mut self,
        group_id: &str,
        continuation_id: &str,
        started_at_ms: u64,
        attempt: u32,
    ) {
        self.ensure_group(group_id, started_at_ms);
        let key = continuation_key(group_id, continuation_id);
        if !self.continuation_lifecycle_attempt_matches(
            &key,
            attempt,
            "continuation_started",
            continuation_id,
        ) {
            return;
        }
        let continuation =
            self.continuations
                .entry(key)
                .or_insert_with(|| PendingContinuationRecord {
                    group_id: group_id.to_string(),
                    continuation_id: continuation_id.to_string(),
                    child_id: None,
                    prompt: None,
                    status: ContinuationStatus::Queued,
                    queued_at_ms: started_at_ms,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    attempt: 0,
                    metadata: SupervisorMetadata::new(),
                });
        if continuation.status != ContinuationStatus::Completed {
            continuation.status = ContinuationStatus::Started;
        }
        continuation.started_at_ms = Some(
            continuation
                .started_at_ms
                .map_or(started_at_ms, |existing| existing.min(started_at_ms)),
        );
    }

    fn apply_continuation_completed(
        &mut self,
        group_id: &str,
        continuation_id: &str,
        completed_at_ms: u64,
        result: Option<String>,
        attempt: u32,
    ) {
        self.ensure_group(group_id, completed_at_ms);
        let key = continuation_key(group_id, continuation_id);
        if !self.continuation_lifecycle_attempt_matches(
            &key,
            attempt,
            "continuation_completed",
            continuation_id,
        ) {
            return;
        }
        let continuation =
            self.continuations
                .entry(key)
                .or_insert_with(|| PendingContinuationRecord {
                    group_id: group_id.to_string(),
                    continuation_id: continuation_id.to_string(),
                    child_id: None,
                    prompt: None,
                    status: ContinuationStatus::Queued,
                    queued_at_ms: completed_at_ms,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    attempt: 0,
                    metadata: SupervisorMetadata::new(),
                });
        continuation.status = ContinuationStatus::Completed;
        continuation.completed_at_ms = Some(
            continuation
                .completed_at_ms
                .map_or(completed_at_ms, |existing| existing.max(completed_at_ms)),
        );
        if result.is_some() {
            continuation.result = result;
        }
    }

    fn ensure_group(&mut self, group_id: &str, observed_at_ms: u64) -> &mut SupervisedGroupRecord {
        self.groups
            .entry(group_id.to_string())
            .or_insert_with(|| SupervisedGroupRecord::new(group_id, observed_at_ms))
    }

    fn remember_child(&mut self, group_id: &str, child_id: &str, observed_at_ms: u64) {
        let group = self.ensure_group(group_id, observed_at_ms);
        let child_was_known = group.child_ids.iter().any(|existing| existing == child_id);
        push_unique(&mut group.child_ids, child_id.to_string());
        if !child_was_known && is_auto_group_terminal(&group.terminal) {
            group.terminal = None;
            group.status = GroupStatus::Running;
            group.updated_at_ms = group.updated_at_ms.max(observed_at_ms);
        } else if group.terminal.is_none() {
            group.status = GroupStatus::Running;
            group.updated_at_ms = group.updated_at_ms.max(observed_at_ms);
        }
    }

    fn recompute_group_terminal(&mut self, group_id: &str) {
        let Some(group) = self.groups.get(group_id) else {
            return;
        };
        if group.child_ids.is_empty()
            || (group.terminal.is_some() && !is_auto_group_terminal(&group.terminal))
        {
            return;
        }

        let mut latest_finished = 0;
        let mut terminal_kind = TerminalKind::Completed;
        for child_id in &group.child_ids {
            let Some(child) = self.children.get(&child_key(group_id, child_id)) else {
                return;
            };
            let Some(terminal) = child.terminal.as_ref() else {
                return;
            };
            latest_finished = latest_finished.max(terminal.finished_at_ms);
            match terminal.kind {
                TerminalKind::Failed => terminal_kind = TerminalKind::Failed,
                TerminalKind::Cancelled if terminal_kind != TerminalKind::Failed => {
                    terminal_kind = TerminalKind::Cancelled;
                }
                TerminalKind::Completed | TerminalKind::Cancelled => {}
            }
        }

        if let Some(group) = self.groups.get_mut(group_id) {
            group.status = group_status_for_terminal(&terminal_kind);
            group.updated_at_ms = group.updated_at_ms.max(latest_finished);
            group.terminal = Some(TerminalState {
                kind: terminal_kind,
                finished_at_ms: latest_finished,
                exit_code: None,
                reason: None,
                message: Some(AUTO_GROUP_TERMINAL_MESSAGE.to_string()),
                metadata: SupervisorMetadata::new(),
            });
        }
    }
}

// This fingerprint follows normal atomic snapshot replacement. It is not a
// content hash: arbitrary external edits preserving identity/length/timestamps
// are outside the filesystem cache contract. Missing modification timestamps
// disable reuse conservatively.
#[derive(Debug, PartialEq, Eq)]
struct SnapshotFingerprint {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64),
}

#[derive(Debug)]
struct ContinuationIndex {
    // Only continuations, global event IDs and last_sequence are retained.
    state: SupervisorState,
    snapshot_cutoff: u64,
    snapshot: Option<SnapshotFingerprint>,
    offset: u64,
    boundary_sequence: Option<u64>,
    extendable: bool,
}

impl ContinuationIndex {
    fn project(state: SupervisorState) -> SupervisorState {
        SupervisorState {
            continuations: state.continuations,
            applied_event_ids: state.applied_event_ids,
            last_sequence: state.last_sequence,
            ..SupervisorState::default()
        }
    }

    fn apply(&mut self, row: &SupervisorEventLedgerRow) {
        // Keep the snapshot cutoff fixed, even for out-of-order raw rows.
        if row.sequence <= self.snapshot_cutoff || !self.state.accept_ledger_row(row) {
            return;
        }
        if matches!(
            row.event,
            SupervisorEvent::ContinuationQueued { .. }
                | SupervisorEvent::ContinuationStarted { .. }
                | SupervisorEvent::ContinuationCompleted { .. }
        ) {
            self.state.apply_event(&row.event, row.recorded_at_ms);
            self.state.groups.clear();
        }
    }
}

/// In-memory cursor over the on-disk ledger, shared by every clone of a store
/// (the orchestrator clones its store into drain loops and tasks). It is only
/// read or written while the cross-process append file-lock is held; the
/// mutex merely guards in-process memory access. A store instance with a cold
/// or stale cursor never mis-assigns a sequence — `refresh_seq_cache_locked`
/// revalidates against the file before every use.
#[derive(Debug, Default)]
struct SeqCache {
    /// False until the first locked use seeds the cursor from disk.
    seeded: bool,
    /// Highest sequence known committed (ledger tail or snapshot).
    last_sequence: u64,
    /// Ledger byte length after the last observed write.
    events_len: u64,
    /// Rows currently in the live ledger (drives auto-compaction).
    ledger_rows: u64,
    /// Appends since the events file was last fsynced (batched-fsync mode).
    appends_since_fsync: u64,
    /// Independent validity: refreshing this projection never changes the
    /// sequence cursor or fsync debt, including AlreadyCompleted returns.
    continuations: Option<ContinuationIndex>,
}

#[derive(Debug, Clone)]
pub struct SupervisorStore {
    root_dir: PathBuf,
    events_path: PathBuf,
    rotated_events_path: PathBuf,
    snapshot_path: PathBuf,
    /// Live-ledger row count that triggers snapshot + compaction on append;
    /// `0` disables auto-compaction.
    snapshot_every_appends: u64,
    /// `Some(n)`: fsync after n sequenced rows (at a batch's end);
    /// `None` (default): appends are never fsynced (see `append_ledger_row`).
    append_fsync_every: Option<u64>,
    seq_cache: Arc<Mutex<SeqCache>>,
}

#[derive(Debug)]
struct SupervisorAppendLock {
    path: PathBuf,
}

impl Drop for SupervisorAppendLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl SupervisorStore {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        let root_dir = root_dir.as_ref().to_path_buf();
        Self {
            events_path: root_dir.join(EVENTS_FILE_NAME),
            rotated_events_path: root_dir.join(EVENTS_ROTATED_FILE_NAME),
            snapshot_path: root_dir.join(SNAPSHOT_FILE_NAME),
            snapshot_every_appends: SNAPSHOT_EVERY_APPENDS,
            append_fsync_every: None,
            seq_cache: Arc::new(Mutex::new(SeqCache::default())),
            root_dir,
        }
    }

    /// Load the current state: snapshot (if any) plus a replay of the ledger
    /// rows newer than the snapshot. Read-only — loading never writes.
    ///
    /// The ledger is read BEFORE the snapshot on purpose: a concurrent
    /// snapshot + compaction (`snapshot_now` / auto-compaction) writes the new
    /// snapshot first and only then rotates the ledger. Reading in the
    /// opposite order could observe the OLD snapshot together with an
    /// ALREADY-ROTATED (empty) ledger and silently drop the rotated window;
    /// ledger-first, either the rows are still in the ledger we read, or the
    /// snapshot we read afterwards already contains them.
    pub fn load_state(&self) -> io::Result<SupervisorState> {
        self.load_state_with_cutoff().map(|(state, _)| state)
    }

    fn load_state_with_cutoff(&self) -> io::Result<(SupervisorState, u64)> {
        let rows = self.read_ledger_rows()?;
        let snapshot = self.load_snapshot()?;
        let snapshot_last_sequence = snapshot.as_ref().map_or(0, |s| s.last_sequence);
        let mut state = snapshot.map_or_else(SupervisorState::default, |s| s.state);
        state.last_sequence = state.last_sequence.max(snapshot_last_sequence);

        for row in rows {
            if row.sequence > snapshot_last_sequence {
                state.apply_ledger_row(&row);
            }
        }
        Ok((state, snapshot_last_sequence))
    }

    /// #26a — goal-scoped view of the stream, folded BY (session, goal) KEY.
    ///
    /// `load_state`'s `state.groups` map is keyed by GROUP id, and every goal
    /// of one session scope shares the SAME `autonomy-goal:<scope>` group — so
    /// the folded map holds only the NEWEST goal of each scope and a
    /// superseded goal (goal_01 replaced by goal_02…) vanishes from
    /// `octos goal list` even though its rows are still in the stream. The
    /// zombie-cleanup path needs to SEE those superseded goals, so this view
    /// scans the raw rows directly (snapshot + ledger tail, same read order
    /// as `load_state`) and folds the LATEST `group_registered` for each
    /// (session_id, goal_id) pair.
    ///
    /// #26a-r1 — the fold key is COMPOSITE: `(session_id, goal_id)`, not the
    /// bare `goal_id`. #25's contract says duplicate goal ids across sessions
    /// are REPORTED, never guessed; a single-key fold let a later session's
    /// registration silently overwrite the earlier one, so `locate_goal`'s
    /// ambiguity scan could never see two. Folding per (session, goal) keeps
    /// both registrations in the map, and `locate_goal`'s values() scan then
    /// counts both and refuses with `ambiguous` as designed. The 26a
    /// zombie-cleanup semantics are unchanged (a superseded goal of the SAME
    /// session still has its own key — the view is only MORE complete).
    ///
    /// The map key ENCODES the pair as `"<session_id>\u{1}<goal_id>"` (unit
    /// separator, impossible in either id) so existing `HashMap<String, _>`
    /// call sites keep compiling; value semantics are per-(session, goal).
    /// Rows that fail to parse are already skipped by the tolerant
    /// replay (#26a).
    pub fn load_goal_groups_by_id(
        &self,
    ) -> io::Result<std::collections::HashMap<String, SupervisedGroupRecord>> {
        let rows = self.read_ledger_rows()?;
        let snapshot = self.load_snapshot()?;
        let snapshot_last_sequence = snapshot.as_ref().map_or(0, |s| s.last_sequence);
        // Fold the snapshot's groups first (they carry sequence context via
        // `last_sequence`), then overlay newer ledger rows. Key is the
        // composite (session_id, goal_id) — see the doc comment above for
        // why the bare goal_id must NOT be the key (#26a-r1).
        let composite_key = |group: &SupervisedGroupRecord| -> Option<String> {
            let goal_id = group.metadata.get("goal_id")?.as_str()?;
            let session_id = group
                .metadata
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(format!("{session_id}\u{1}{goal_id}"))
        };
        let mut by_goal: std::collections::HashMap<String, SupervisedGroupRecord> =
            std::collections::HashMap::new();
        let mut order: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        if let Some(snapshot) = snapshot.as_ref() {
            for group in snapshot.state.groups.values() {
                if let Some(key) = composite_key(group) {
                    order.insert(key.clone(), snapshot_last_sequence);
                    by_goal.insert(key, group.clone());
                }
            }
        }
        for row in rows {
            if row.sequence <= snapshot_last_sequence {
                continue;
            }
            if let SupervisorEvent::GroupRegistered { group } = &row.event {
                if let Some(key) = composite_key(group) {
                    let slot = order.entry(key.clone()).or_insert(0);
                    if row.sequence >= *slot {
                        *slot = row.sequence;
                        by_goal.insert(key, group.clone());
                    }
                }
            }
        }
        Ok(by_goal)
    }

    /// Load the snapshot, refusing one written by a NEWER binary: a
    /// `schema_version` above what this build knows means fields we would
    /// silently drop or misread, and a snapshot is the authoritative record
    /// of compacted history — misinterpreting it corrupts state. (Downgrade
    /// to a snapshot-UNAWARE binary is different: such builds ignore the
    /// snapshot file entirely and replay only the live tail — see
    /// `EVENTS_ROTATED_FILE_NAME` for what remains recoverable.)
    pub fn load_snapshot(&self) -> io::Result<Option<SupervisorSnapshot>> {
        let body = match fs::read_to_string(&self.snapshot_path) {
            Ok(body) => body,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let snapshot: SupervisorSnapshot = serde_json::from_str(&body).map_err(invalid_data)?;
        if snapshot.schema_version > SNAPSHOT_SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "supervisor snapshot {} has schema_version {} but this binary supports <= {}; \
                     upgrade the binary, or move the snapshot aside to fall back to the live \
                     ledger tail",
                    self.snapshot_path.display(),
                    snapshot.schema_version,
                    SNAPSHOT_SCHEMA_VERSION
                ),
            ));
        }
        Ok(Some(snapshot))
    }

    /// Append an event, assigning the next ledger sequence.
    ///
    /// The sequence comes from the in-memory cursor, revalidated against the
    /// file under the append lock (`refresh_seq_cache_locked`) — O(tail
    /// probe) in the common case instead of the historical full-JSONL
    /// re-parse per append. Every [`SNAPSHOT_EVERY_APPENDS`] ledger rows this
    /// also snapshots + compacts the ledger (best effort: a failed compaction
    /// never fails the already-durable append; the threshold stays exceeded
    /// so the next append retries).
    pub fn append_event(
        &self,
        event_id: impl Into<String>,
        event: SupervisorEvent,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let _lock = self.acquire_append_lock()?;
        let mut cache = self.lock_seq_cache();
        self.append_event_locked(event_id.into(), event, &mut cache)
    }

    // Caller holds the append-file lock, then the sequence-cache guard.
    fn append_event_locked(
        &self,
        mut event_id: String,
        event: SupervisorEvent,
        cache: &mut SeqCache,
    ) -> io::Result<SupervisorEventLedgerRow> {
        self.refresh_seq_cache_locked(cache)?;
        let sequence = cache.last_sequence.saturating_add(1);
        if event_id.is_empty() {
            event_id = format!("event:{sequence}");
        }
        let row = SupervisorEventLedgerRow {
            event_id,
            sequence,
            recorded_at_ms: unix_time_millis(),
            event,
        };
        self.append_row_locked(&row, cache)?;
        if self.snapshot_every_appends > 0 && cache.ledger_rows >= self.snapshot_every_appends {
            if let Err(err) = self.snapshot_and_compact_locked(cache) {
                tracing::warn!(
                    error = %err,
                    events_path = %self.events_path.display(),
                    "supervisor ledger auto-compaction failed; will retry on a later append"
                );
            }
        }
        Ok(row)
    }

    /// Commit the complete child and optional cohort epoch in one row.
    /// A post-write I/O error may leave this complete event durable, but
    /// replay can never observe only one half of its child/epoch payload.
    pub fn record_agent_admitted(
        &self,
        group: SupervisedGroupRecord,
        child: ChildAgentRecord,
        cohort: Option<CohortEpochAdmission>,
    ) -> io::Result<SupervisorEventLedgerRow> {
        if group.group_id != child.group_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "agent admission group does not match the child's group",
            ));
        }
        // The locked sequence allocator supplies a fresh event id. Unlike
        // ChildStarted's stable id, it allows later checked child updates,
        // including multiple updates within the same millisecond.
        self.append_event(
            "",
            SupervisorEvent::AgentAdmitted {
                group,
                child,
                cohort,
            },
        )
    }

    pub fn record_heartbeat(&self, ping: HeartbeatPing) -> io::Result<SupervisorEventLedgerRow> {
        let ping_part = ping
            .ping_id
            .as_deref()
            .map_or_else(|| ping.observed_at_ms.to_string(), ToString::to_string);
        let event_id = format!("heartbeat:{}:{}:{ping_part}", ping.group_id, ping.child_id);
        self.append_event(event_id, SupervisorEvent::Heartbeat { ping })
    }

    pub fn record_artifact_updated(
        &self,
        artifact: ArtifactRecord,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let event_id = format!(
            "artifact_updated:{}:{}:{}",
            artifact.group_id, artifact.artifact_id, artifact.version
        );
        self.append_event(event_id, SupervisorEvent::ArtifactUpdated { artifact })
    }

    pub fn record_continuation_queued(
        &self,
        continuation: PendingContinuationRecord,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let event_id = format!(
            "continuation_queued:{}:{}:{}",
            continuation.group_id, continuation.continuation_id, continuation.attempt
        );
        self.append_event(
            event_id,
            SupervisorEvent::ContinuationQueued { continuation },
        )
    }

    /// Check replayed state and append under the same cross-process lock.
    /// Completed is final for retry callers, even for a higher offered attempt.
    pub fn record_continuation_queued_if_not_completed(
        &self,
        continuation: PendingContinuationRecord,
    ) -> io::Result<ContinuationQueueOutcome> {
        if continuation.status != ContinuationStatus::Queued {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected Queued continuation",
            ));
        }
        let _lock = self.acquire_append_lock()?;
        let mut cache = self.lock_seq_cache();
        self.refresh_continuations_locked(&mut cache)?;
        let key = continuation_key(&continuation.group_id, &continuation.continuation_id);
        if let Some(current) = cache
            .continuations
            .as_ref()
            .and_then(|index| index.state.continuations.get(&key))
            && current.status == ContinuationStatus::Completed
        {
            return Ok(ContinuationQueueOutcome::AlreadyCompleted(current.clone()));
        }
        let event_id = format!(
            "continuation_queued:{}:{}:{}",
            continuation.group_id, continuation.continuation_id, continuation.attempt
        );
        self.append_event_locked(
            event_id,
            SupervisorEvent::ContinuationQueued { continuation },
            &mut cache,
        )
        .map(ContinuationQueueOutcome::Written)
    }

    pub fn record_continuation_started(
        &self,
        group_id: impl Into<String>,
        continuation_id: impl Into<String>,
        started_at_ms: u64,
        attempt: u32,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let group_id = group_id.into();
        let continuation_id = continuation_id.into();
        let event_id =
            format!("continuation_started:{group_id}:{continuation_id}:{started_at_ms}:{attempt}");
        self.append_event(
            event_id,
            SupervisorEvent::ContinuationStarted {
                group_id,
                continuation_id,
                started_at_ms,
                attempt,
            },
        )
    }

    pub fn record_continuation_completed(
        &self,
        group_id: impl Into<String>,
        continuation_id: impl Into<String>,
        completed_at_ms: u64,
        result: Option<String>,
        attempt: u32,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let group_id = group_id.into();
        let continuation_id = continuation_id.into();
        let event_id = format!(
            "continuation_completed:{group_id}:{continuation_id}:{completed_at_ms}:{attempt}"
        );
        self.append_event(
            event_id,
            SupervisorEvent::ContinuationCompleted {
                group_id,
                continuation_id,
                completed_at_ms,
                result,
                attempt,
            },
        )
    }

    /// One coalesced-fold tombstone (a folded extra, marked `Completed` with
    /// `result` naming the carrier it folded into).
    ///
    /// #1707 round 4 (codex Should-fix 3): the terminal-coalesce fold used to
    /// issue one `record_continuation_completed` per extra — N independent
    /// append-lock acquisitions serialised inside the orchestrator state
    /// lock. [`SupervisorStore::record_continuations_coalesced`] now appends
    /// the whole batch under ONE append lock with one ledger open,
    /// `write_all`, and flush (one lock wait, at most one snapshot compaction).
    /// `/stop` tombstones use this same batch path.
    pub fn record_continuations_coalesced(
        &self,
        entries: &[CoalescedTombstoneEntry],
    ) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        // Single-append-lock batch: mirror `append_event`'s lock + seq-cache
        // pattern, then assign consecutive sequences and write one payload.
        // Every entry keeps its own STABLE event id (keyed on the
        // continuation id + completed_at_ms) so replay's `applied_event_ids`
        // dedup makes a partial batch + caller retry idempotent per entry.
        let _lock = self.acquire_append_lock()?;
        let mut cache = self.lock_seq_cache();
        self.refresh_seq_cache_locked(&mut cache)?;
        let mut sequence = cache.last_sequence;
        let rows: Vec<_> = entries
            .iter()
            .map(|entry| {
                sequence = sequence.saturating_add(1);
                SupervisorEventLedgerRow {
                    event_id: format!(
                        "continuation_completed:{}:{}:{}",
                        entry.group_id, entry.continuation_id, entry.completed_at_ms
                    ),
                    sequence,
                    recorded_at_ms: unix_time_millis(),
                    event: SupervisorEvent::ContinuationCompleted {
                        group_id: entry.group_id.clone(),
                        continuation_id: entry.continuation_id.clone(),
                        completed_at_ms: entry.completed_at_ms,
                        result: Some(entry.result.clone()),
                        attempt: entry.attempt,
                    },
                }
            })
            .collect();
        let result = self.append_rows_locked(&rows, &mut cache);
        if self.snapshot_every_appends > 0 && cache.ledger_rows >= self.snapshot_every_appends {
            if let Err(err) = self.snapshot_and_compact_locked(&mut cache) {
                // Mirror `append_event`: a failed compaction never fails the
                // already-durable appends; the threshold stays exceeded so a
                // later append retries.
                tracing::warn!(
                    error = %err,
                    events_path = %self.events_path.display(),
                    "supervisor ledger auto-compaction failed; will retry on a later append"
                );
            }
        }
        result?;
        Ok(())
    }

    /// Whole-line batch append (append lock must be held — EVERY events-file write
    /// goes through here under the lock; an unlocked write could land between
    /// a concurrent compaction's "snapshot rows" and "rotate ledger" steps
    /// and be rotated away unreplayed). Returns the handle so callers can
    /// fsync it.
    ///
    /// Two repairs happen inline:
    /// - Torn tail: if the file is non-empty and does not end in a newline
    ///   (a crash split an earlier append), a `\n` is written first so this
    ///   batch can never concatenate onto the torn content. The torn bytes are
    ///   preserved, never truncated — they may be a complete row that merely
    ///   lost its terminator.
    /// - Fresh file: when this write CREATES the ledger (fresh store, or the
    ///   first append after a compaction rotated it away), the parent
    ///   directory is fsynced so the new name is durable — without this the
    ///   batched append-fsync bound would be void (power loss could keep the
    ///   snapshot but lose the new ledger's directory entry).
    ///
    /// All rows and their terminators share one `write_all`. A failed write
    /// can still persist a prefix; replay tolerates a torn final row and
    /// stable event IDs suppress already-written entries on caller retry.
    fn write_rows_sealed_locked(&self, rows: &[SupervisorEventLedgerRow]) -> io::Result<File> {
        self.ensure_root_dir()?;
        let created = !self.events_path.exists();
        // #39 — the ledger append OPEN also sits on the compaction-adjacent
        // path (the file was just rotated; Windows reopens can hit the same
        // transient spectrum). Retry-wrapped on Windows, single-shot on unix.
        #[cfg(windows)]
        let file = fs_retry(
            &RetryClock::default(),
            std::time::Duration::from_millis(fs_retry_total_ms()),
            || {
                fs_ctx(
                    "events-append-open",
                    &self.events_path,
                    OpenOptions::new()
                        .read(true)
                        .create(true)
                        .append(true)
                        .open(&self.events_path),
                )
            },
            is_transient_windows_lock,
        )?;
        #[cfg(not(windows))]
        let file = fs_ctx(
            "events-append-open",
            &self.events_path,
            OpenOptions::new()
                .read(true)
                .create(true)
                .append(true)
                .open(&self.events_path),
        )?;
        let mut file = file;
        #[cfg(windows)]
        let len = fs_retry(
            &RetryClock::default(),
            std::time::Duration::from_millis(fs_retry_total_ms()),
            || fs_ctx("events-metadata", &self.events_path, file.metadata()),
            is_transient_windows_lock,
        )?
        .len();
        #[cfg(not(windows))]
        let len = fs_ctx("events-metadata", &self.events_path, file.metadata())?.len();
        let mut payload = Vec::with_capacity(256);
        if len > 0 {
            file.seek(SeekFrom::Start(len - 1))?;
            let mut last = [0_u8; 1];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                payload.push(b'\n');
            }
        }
        for row in rows {
            serde_json::to_writer(&mut payload, row).map_err(invalid_data)?;
            payload.push(b'\n');
        }
        // O_APPEND: the write lands at EOF regardless of the read seek above.
        fs_ctx(
            "events-row-write",
            &self.events_path,
            file.write_all(&payload),
        )?;
        fs_ctx("events-row-flush", &self.events_path, file.flush())?;
        if created {
            fs_ctx(
                "events-create-dir-fsync",
                &self.root_dir,
                fsync_dir(&self.root_dir),
            )?;
        }
        Ok(file)
    }

    fn append_row_locked(
        &self,
        row: &SupervisorEventLedgerRow,
        cache: &mut SeqCache,
    ) -> io::Result<()> {
        self.append_rows_locked(std::slice::from_ref(row), cache)
    }

    /// Write one sequenced batch, apply the fsync policy, and advance the
    /// cursor only after the whole write succeeds. On any I/O error the next
    /// append reseeds from disk, including a possibly persisted prefix.
    fn append_rows_locked(
        &self,
        rows: &[SupervisorEventLedgerRow],
        cache: &mut SeqCache,
    ) -> io::Result<()> {
        let Some(last) = rows.last() else {
            return Ok(());
        };
        let row_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        // Move, never clone the growing index. Keep its old checkpoint after
        // success so the next guard consumes local AND foreign suffix rows in
        // order. Any uncertain write/sync/metadata failure leaves it invalid.
        let continuations = cache.continuations.take();
        cache.seeded = false;
        // Count conservatively before I/O: even a failed write/flush may
        // leave rows on disk. Keep their fsync debt until a successful sync
        // so partial batches cannot silently extend the durability window.
        if self.append_fsync_every.is_some() {
            cache.appends_since_fsync = cache.appends_since_fsync.saturating_add(row_count);
        }
        let file = self.write_rows_sealed_locked(rows)?;
        if let Some(every) = self.append_fsync_every {
            if cache.appends_since_fsync >= every {
                file.sync_data()?;
                cache.appends_since_fsync = 0;
            }
        }
        // Exact length of the file we just extended; nothing else can write
        // while we hold the lock.
        let events_len = file.metadata()?.len();
        // #34f — release the events-file handle BEFORE the caller may
        // compact (metadata read above must come first): on Windows,
        // renaming a file that still has an open handle fails, and
        // `append_event`'s auto-compaction renames exactly this file.
        // POSIX tolerates the open handle — the five snapshot tests were
        // green on Linux and Os error 3 on Windows for exactly this reason.
        drop(file);
        cache.last_sequence = cache.last_sequence.max(last.sequence);
        cache.events_len = events_len;
        cache.ledger_rows = cache.ledger_rows.saturating_add(row_count);
        cache.seeded = true;
        cache.continuations = continuations;
        Ok(())
    }

    /// Snapshot writer (append lock must be held): atomic tmp + rename, with
    /// the file fsynced before the rename and the directory fsynced after.
    /// The fsyncs are load-bearing — a snapshot licenses compaction to delete
    /// the ledger rows it covers, so it must be physically durable first.
    fn write_snapshot_locked(&self) -> io::Result<SupervisorSnapshot> {
        let state = self.load_state()?;
        // `applied_event_ids` is retained in FULL across snapshots — it is
        // the DURABLE dedup contract, not tail-epoch bookkeeping. The
        // sequence cutoff only suppresses rows at or below the snapshot's
        // `last_sequence`; a duplicate STABLE event id re-emitted at a HIGHER
        // sequence (e.g. `record_group_registered` reuses
        // `group_registered:<group_id>` verbatim) is suppressed only by this
        // set, and re-applying it would clobber state that the id had
        // already been suppressed for before the snapshot. RESIDUAL
        // (documented, accepted): the set grows O(unique event ids) over the
        // store's lifetime, and snapshots/boot parses grow with it. Pruning
        // would require a per-kind id-stability audit; the current audit says
        // NOTHING is provably prunable — every producer's id format can
        // legitimately recur (`group_registered:<gid>` and
        // `child_started:<gid>:<cid>` are stable by design; terminal ids key
        // on `(kind, finished_at_ms)`; heartbeat ids embed a caller-owned
        // `ping_id`; artifact ids key on `version`; continuation ids key on
        // `attempt`/timestamps; the `event:<seq>` fallback trusts raw writers
        // not to reuse sequences). Deferred until some kind gains a provably
        // unique id.
        let snapshot = SupervisorSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            written_at_ms: unix_time_millis(),
            last_sequence: state.last_sequence,
            state,
        };
        self.ensure_root_dir()?;
        let body = serde_json::to_string_pretty(&snapshot).map_err(invalid_data)?;
        let tmp_path = self.snapshot_path.with_extension("json.tmp");
        // #34g — via rename_replace: on a SECOND snapshot the destination
        // already exists, and a bare fs::rename fails on Windows (POSIX
        // replaces). This was the error-2 (B) cluster's primary suspect.
        // #39 — the tmp write steps also sit on the snapshot path and see
        // the same transient-lock spectrum (AV/indexer probes on freshly
        // created files). Route through the retry helper on Windows.
        #[cfg(windows)]
        {
            let tmp_path = &tmp_path;
            let body_bytes = body.as_bytes();
            fs_retry(
                &RetryClock::default(),
                std::time::Duration::from_millis(fs_retry_total_ms()),
                || {
                    fs_ctx("snapshot-tmp-create", tmp_path, File::create(tmp_path)).and_then(
                        |mut tmp| {
                            fs_ctx("snapshot-tmp-write", tmp_path, tmp.write_all(body_bytes))
                                .and_then(|()| {
                                    fs_ctx("snapshot-tmp-sync", tmp_path, tmp.sync_all())
                                })
                        },
                    )
                },
                is_transient_windows_lock,
            )?;
        }
        #[cfg(not(windows))]
        {
            let mut tmp = fs_ctx("snapshot-tmp-create", &tmp_path, File::create(&tmp_path))?;
            fs_ctx(
                "snapshot-tmp-write",
                &tmp_path,
                tmp.write_all(body.as_bytes()),
            )?;
            fs_ctx("snapshot-tmp-sync", &tmp_path, tmp.sync_all())?;
        }
        fs_ctx(
            "snapshot-rename-replace",
            &self.snapshot_path,
            rename_replace(&tmp_path, &self.snapshot_path),
        )?;
        fs_ctx(
            "snapshot-dir-fsync",
            &self.root_dir,
            fsync_dir(&self.root_dir),
        )?;
        Ok(snapshot)
    }

    /// Second half of the compaction cycle (append lock must be held): once
    /// the snapshot is durable, rotate the fully-applied ledger to the single
    /// `.jsonl.old` forensics generation and reset the cursor. On any partial
    /// failure the cursor is left stale, which the next append detects and
    /// repairs by reseeding from disk.
    fn snapshot_and_compact_locked(&self, cache: &mut SeqCache) -> io::Result<SupervisorSnapshot> {
        cache.continuations = None;
        let snapshot = self.write_snapshot_locked()?;
        if self.events_path.exists() {
            // #34g — rename_replace handles the previous generation
            // (remove-then-rename; see the helper for the crash-window
            // analysis).
            fs_ctx(
                "events-rotate-rename-replace",
                &self.rotated_events_path,
                rename_replace(&self.events_path, &self.rotated_events_path),
            )?;
            fs_ctx(
                "compact-dir-fsync",
                &self.root_dir,
                fsync_dir(&self.root_dir),
            )?;
        }
        cache.seeded = true;
        cache.last_sequence = cache.last_sequence.max(snapshot.last_sequence);
        cache.events_len = 0;
        cache.ledger_rows = 0;
        // The snapshot fsync covered everything the ledger held.
        cache.appends_since_fsync = 0;
        cache.continuations = Some(ContinuationIndex {
            state: SupervisorState {
                continuations: snapshot.state.continuations.clone(),
                applied_event_ids: snapshot.state.applied_event_ids.clone(),
                last_sequence: snapshot.state.last_sequence,
                ..SupervisorState::default()
            },
            snapshot_cutoff: snapshot.last_sequence,
            snapshot: self.snapshot_fingerprint()?,
            offset: 0,
            boundary_sequence: None,
            extendable: true,
        });
        Ok(snapshot)
    }

    fn lock_seq_cache(&self) -> MutexGuard<'_, SeqCache> {
        match self.seq_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // A panic while holding the guard may have left a
                // half-updated cursor; force a reseed from disk on next use.
                let mut guard = poisoned.into_inner();
                guard.seeded = false;
                guard.continuations = None;
                guard
            }
        }
    }

    fn snapshot_fingerprint(&self) -> io::Result<Option<SnapshotFingerprint>> {
        let metadata = match fs::metadata(&self.snapshot_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Some(SnapshotFingerprint {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            identity: (metadata.dev(), metadata.ino()),
        }))
    }

    // The append lock is held throughout validation, catch-up, check and write.
    // Take the index before fallible reads; never publish partial replay after
    // an error and never clone the whole index on the ordinary warm path.
    fn refresh_continuations_locked(&self, cache: &mut SeqCache) -> io::Result<()> {
        let previous = cache.continuations.take();
        let snapshot = self.snapshot_fingerprint()?;
        let disk_len = match fs::metadata(&self.events_path) {
            Ok(metadata) => metadata.len(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err),
        };
        let mut index = match previous {
            Some(mut index)
                if index.snapshot == snapshot
                    && snapshot
                        .as_ref()
                        .is_none_or(|stamp| stamp.modified.is_some())
                    && index.extendable
                    && disk_len >= index.offset
                    && (index.offset == 0
                        || (index.boundary_sequence.is_some()
                            && self.read_last_row_sequence(index.offset)?
                                == index.boundary_sequence)) =>
            {
                if disk_len == index.offset {
                    cache.continuations = Some(index);
                    return Ok(());
                }
                for row in self.read_ledger_rows_from(index.offset)? {
                    index.apply(&row);
                }
                index
            }
            _ => {
                let (state, snapshot_cutoff) = self.load_state_with_cutoff()?;
                ContinuationIndex {
                    state: ContinuationIndex::project(state),
                    snapshot_cutoff,
                    snapshot,
                    offset: 0,
                    boundary_sequence: None,
                    extendable: false,
                }
            }
        };
        index.offset = disk_len;
        index.boundary_sequence = self.read_last_row_sequence(disk_len)?;
        index.extendable = if disk_len == 0 {
            true
        } else {
            let mut file = File::open(&self.events_path)?;
            file.seek(SeekFrom::Start(disk_len - 1))?;
            let mut byte = [0];
            file.read_exact(&mut byte)?;
            byte[0] == b'\n'
        };
        cache.continuations = Some(index);
        Ok(())
    }

    /// Bring the in-memory cursor in line with the on-disk ledger. Must be
    /// called under the append file-lock, so what it observes cannot change
    /// until the lock is released.
    ///
    /// Fast path: the ledger's byte length is unchanged AND its final row
    /// still carries our cached sequence (one bounded tail read). Both checks
    /// are required — length alone would let a foreign compact-then-append
    /// cycle that lands on the same byte length (an ABA) masquerade as "no
    /// change", and the final sequence alone would miss growth. Under the
    /// locking protocol any sequenced writer that touches the ledger changes
    /// its length and/or its final sequence; raw `append_ledger_row` writers
    /// additionally invalidate their own process's cursor outright.
    ///
    /// ANY other observation — first use, growth, shrink/rotation, tail
    /// mismatch, unparseable tail — takes a full reseed. Compaction keeps the
    /// ledger at most ~[`SNAPSHOT_EVERY_APPENDS`] rows, so reseeding is cheap;
    /// boring-correct beats a cleverer partial rescan here.
    fn refresh_seq_cache_locked(&self, cache: &mut SeqCache) -> io::Result<()> {
        let disk_len = match fs::metadata(&self.events_path) {
            Ok(meta) => meta.len(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err),
        };

        if cache.seeded
            && disk_len > 0
            && disk_len == cache.events_len
            && self.read_last_row_sequence(disk_len)? == Some(cache.last_sequence)
        {
            return Ok(());
        }
        self.reseed_cache_locked(cache, disk_len)
    }

    /// Rebuild the cursor from disk: `max(snapshot.last_sequence, max row
    /// sequence in the ledger)`. Under the lock, disk is authoritative — any
    /// row this process ever appended is covered by the ledger or by a
    /// snapshot that compacted it.
    fn reseed_cache_locked(&self, cache: &mut SeqCache, disk_len: u64) -> io::Result<()> {
        let snapshot_last = self
            .load_snapshot()?
            .map_or(0, |snapshot| snapshot.last_sequence);
        let rows = self.read_ledger_rows()?;
        let ledger_max = rows.iter().map(|row| row.sequence).max().unwrap_or(0);
        cache.last_sequence = snapshot_last.max(ledger_max);
        cache.events_len = disk_len;
        cache.ledger_rows = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        cache.seeded = true;
        Ok(())
    }

    /// Parse the sequence of the final non-empty ledger line, reading only a
    /// bounded tail window (doubled until it covers a whole line). Returns
    /// `Ok(None)` when the tail is not a parseable row (torn write, foreign
    /// truncation); callers then fall back to a full reseed, which reports
    /// corruption with precise line context.
    fn read_last_row_sequence(&self, disk_len: u64) -> io::Result<Option<u64>> {
        let mut file = match File::open(&self.events_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let mut window = TAIL_PROBE_BYTES;
        loop {
            let start = disk_len.saturating_sub(window);
            let span = usize::try_from(disk_len - start).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ledger tail window exceeds addressable memory",
                )
            })?;
            file.seek(SeekFrom::Start(start))?;
            let mut buf = vec![0_u8; span];
            file.read_exact(&mut buf)?;

            let content_end = buf
                .iter()
                .rposition(|byte| !byte.is_ascii_whitespace())
                .map_or(0, |idx| idx + 1);
            if content_end == 0 {
                if start == 0 {
                    // Whitespace-only file: no rows.
                    return Ok(None);
                }
                window = window.saturating_mul(2);
                continue;
            }
            let content = &buf[..content_end];
            match content.iter().rposition(|&byte| byte == b'\n') {
                Some(newline) => {
                    let line = &content[newline + 1..];
                    return Ok(serde_json::from_slice::<SupervisorEventLedgerRow>(line)
                        .ok()
                        .map(|row| row.sequence));
                }
                None if start == 0 => {
                    return Ok(serde_json::from_slice::<SupervisorEventLedgerRow>(content)
                        .ok()
                        .map(|row| row.sequence));
                }
                None => {
                    // The final line starts before this window; widen it.
                    window = window.saturating_mul(2);
                }
            }
        }
    }

    fn read_ledger_rows(&self) -> io::Result<Vec<SupervisorEventLedgerRow>> {
        self.read_ledger_rows_from(0)
    }

    fn read_ledger_rows_from(&self, offset: u64) -> io::Result<Vec<SupervisorEventLedgerRow>> {
        // Open-and-match instead of exists()-then-open: a concurrent
        // compaction may rotate the ledger away between the two, which must
        // read as "no rows", not an error.
        // #34h — handle-lifetime self-audit: the file handle (and its
        // BufReader) live ONLY inside this function's scope and drop on
        // return, so no in-process handle overlaps the rename_replace
        // window. `load_snapshot` uses `fs::read_to_string`, whose handle
        // is opened-and-closed inside the call. Kept explicit so a future
        // refactor that hoists these handles shows up against this comment.
        let mut file = match File::open(&self.events_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        file.seek(SeekFrom::Start(offset))?;
        let reader = BufReader::new(file);
        let mut rows = Vec::new();
        // #26a — tolerant replay: a SINGLE malformed line (a torn write from a
        // crash, a hand-appended row with a subtly wrong shape, an upgraded
        // schema's legacy row) must not poison the WHOLE stream — the goals a
        // CLI/orchestrator can see would silently drop to whatever fallback
        // path loads (observed live: `octos goal list` on a stream with one
        // bad row showed only the newest goal). Skip the bad line with a warn
        // naming its index; a stream whose rows are ALL bad still yields an
        // empty replay, which callers already handle.
        let mut skipped = 0usize;
        for (idx, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(row) => {
                    rows.push(row);
                }
                Err(err) => {
                    skipped += 1;
                    tracing::warn!(
                        target: "octos::supervisor",
                        path = %self.events_path.display(),
                        replay_offset = offset,
                        // `replay_line` is relative to `replay_offset`.
                        replay_line = idx + 1,
                        error = %err,
                        "skipping malformed supervisor event row (#26a tolerant replay)"
                    );
                }
            }
        }
        if skipped > 0 {
            tracing::warn!(
                target: "octos::supervisor",
                path = %self.events_path.display(),
                skipped,
                loaded = rows.len(),
                "supervisor event stream replay skipped malformed rows"
            );
        }
        Ok(rows)
    }

    fn ensure_root_dir(&self) -> io::Result<()> {
        fs_ctx(
            "ensure-root-dir",
            &self.root_dir,
            fs::create_dir_all(&self.root_dir),
        )
    }

    fn acquire_append_lock(&self) -> io::Result<SupervisorAppendLock> {
        self.ensure_root_dir()?;
        let path = self.root_dir.join(EVENTS_LOCK_FILE_NAME);
        let deadline = Instant::now() + APPEND_LOCK_TIMEOUT;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "pid={}", std::process::id())?;
                    return Ok(SupervisorAppendLock { path });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!(
                                "timed out acquiring supervisor event ledger lock: {}",
                                path.display()
                            ),
                        ));
                    }
                    std::thread::sleep(APPEND_LOCK_RETRY_DELAY);
                }
                Err(err) => return Err(err),
            }
        }
    }
}

/// #34g — cross-platform REPLACE-ON-EXISTING rename.
///
/// Windows `std::fs::rename` FAILS when the destination exists (POSIX
/// replaces atomically). Both compaction-rename sites in this store target
/// files that legitimately already exist on a second cycle (the snapshot
/// overwrites the previous snapshot; the rotated ledger overwrites the
/// previous generation), so every rename here goes through this helper:
/// remove the destination first (tolerating NotFound — the first cycle),
/// then rename. The remove/rename pair is NOT atomic on either platform,
/// but both callers write crash-recoverable layouts (a missing snapshot
/// falls back to full replay; a missing rotation leaves the live ledger),
/// so the tiny window is absorbed by the recovery paths, not by durability.
/// #34h — is this error a TRANSIENT Windows file-lock condition worth
/// retrying? The live winlab spectrum (fifth round): 183 AlreadyExists,
/// 5 AccessDenied, 3 NotFound during the remove→rename pair — delete-pending
/// windows and AV/indexer transient locks. Unix never hits these (rename is
/// atomic replace), so the retry is Windows-only and this predicate is
/// compiled out elsewhere.
#[cfg(windows)]
fn is_transient_windows_lock(err: &io::Error) -> bool {
    use io::ErrorKind::*;
    matches!(err.kind(), PermissionDenied | AlreadyExists)
        || err.raw_os_error().is_some_and(|code| code == 32) // ERROR_SHARING_VIOLATION
}

/// ###27-B2 — injectable file ops so tests can mechanically pin the
/// rename_replace error-classification contract (persistent
/// PermissionDenied is never success-ified; remove's non-NotFound real
/// error returns immediately; retry exhaustion returns the LAST observed
/// error). Production uses the real fs; tests swap in scripted outcomes.
/// #40 — wrap a fallible fs op's error with op name + full path + errno,
/// so a test panic prints the exact culprit path (winlab forensics: the
/// victim set moves and the error spectrum alone can't name the offender).
/// Zero behavior change: only the error TEXT gains context; kinds and
/// results pass through untouched. `source` is chained for full fidelity.
fn fs_ctx<T>(op: &'static str, path: &Path, r: io::Result<T>) -> io::Result<T> {
    r.map_err(|err| {
        let errno = err
            .raw_os_error()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into());
        io::Error::new(
            err.kind(),
            format!(
                "[fs:{op}] path={} errno={errno} source={err}",
                path.display()
            ),
        )
    })
}

/// #39 — total retry budget, environment-tunable. Default ~2.5s (the
/// 34h bound); CI's slow runners can raise it (e.g. 10s) via
/// `OCTOS_FS_RETRY_TOTAL_MS`. Read once per call (cheap env read, no lock).
#[cfg(windows)]
fn fs_retry_total_ms() -> u64 {
    std::env::var("OCTOS_FS_RETRY_TOTAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(2_500)
}

/// #39 — run ONE fallible fs operation under the transient-lock retry
/// policy (Windows only; unix is single-shot). Used for EVERY fs mutation
/// on the snapshot/compaction path so the whole path shares one budget
/// class, not just the two rename sites.
/// #42 — injectable retry clock: `now` for elapsed-time bookkeeping and
/// `sleep` for the backoff pause. Production passes the real pair
/// (semantics unchanged: real Instant + real thread sleep, budget from
/// OCTOS_FS_RETRY_TOTAL_MS); tests pass a virtual clock so attempt counts
/// are DERIVED from virtual time, not the machine's speed (the round-8
/// winlab reds were slow runners eating the 2.5s budget with real sleeps
/// before the 10th attempt).
#[cfg(windows)]
pub(crate) struct RetryClock {
    pub(crate) now: NowFn,
    pub(crate) sleep: SleepFn,
}

#[cfg(windows)]
impl Default for RetryClock {
    fn default() -> Self {
        let start = std::time::Instant::now();
        Self {
            now: Box::new(move || start.elapsed()),
            sleep: Box::new(std::thread::sleep),
        }
    }
}

#[cfg(windows)]
fn fs_retry<T>(
    clock: &RetryClock,
    total: std::time::Duration,
    op: impl Fn() -> io::Result<T>,
    is_transient: impl Fn(&io::Error) -> bool,
) -> io::Result<T> {
    // #40 — callers wrap `op` with fs_ctx so retries and the final return
    // both carry op+path+errno context (the transient judge reads the
    // KIND, which fs_ctx preserves).
    let start = (clock.now)();
    let mut delay = 20u64;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(err) if is_transient(&err) => {
                if (clock.now)().saturating_sub(start) >= total {
                    return Err(err); // budget exhausted — LAST observed error
                }
                (clock.sleep)(std::time::Duration::from_millis(delay));
                delay = delay.saturating_mul(2);
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(windows)]
type NowFn = Box<dyn Fn() -> std::time::Duration + Send + Sync>;
type RemoveFn = Box<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;
type RenameFn = Box<dyn Fn(&Path, &Path) -> io::Result<()> + Send + Sync>;
#[cfg(windows)]
type SleepFn = Box<dyn Fn(std::time::Duration) + Send + Sync>;

pub(crate) struct ReplaceOps {
    pub(crate) remove: RemoveFn,
    pub(crate) rename: RenameFn,
    #[cfg(windows)]
    pub(crate) is_transient: Box<dyn Fn(&io::Error) -> bool + Send + Sync>,
    /// #42 — retry clock + budget; default = real clock + env budget
    /// (production semantics unchanged). Tests inject a virtual clock and
    /// an explicit budget so counts derive from VIRTUAL time.
    #[cfg(windows)]
    pub(crate) clock: RetryClock,
    #[cfg(windows)]
    pub(crate) budget: std::time::Duration,
}

impl Default for ReplaceOps {
    fn default() -> Self {
        Self {
            remove: Box::new(|p: &Path| fs::remove_file(p)),
            rename: Box::new(|s: &Path, d: &Path| fs::rename(s, d)),
            #[cfg(windows)]
            is_transient: Box::new(is_transient_windows_lock),
            #[cfg(windows)]
            clock: RetryClock::default(),
            #[cfg(windows)]
            budget: std::time::Duration::from_millis(fs_retry_total_ms()),
        }
    }
}

/// #34g/#34h — cross-platform REPLACE-ON-EXISTING rename with a BOUNDED
/// retry on Windows transient locks.
///
/// Windows `std::fs::rename` fails when the destination exists (POSIX
/// replaces atomically), so the destination is removed first. Fifth-round
/// winlab evidence showed the remove→rename pair also colliding with
/// delete-pending windows and transient AV/indexer locks, so on Windows
/// both steps retry on PermissionDenied / AlreadyExists / os error 32 with
/// a 20ms-start exponential backoff, at most 10 attempts (~2s total budget;
/// 20+40+80+160+320+640+1280 = 2540ms worst case) before the original error
/// is returned. Unix keeps the single-shot semantics — the retry arm is
/// cfg'd out entirely.
///
/// A remove error that is NOT NotFound (and not, on Windows, transient) is
/// returned immediately — never silently swallowed.
fn rename_replace(src: &Path, dst: &Path) -> io::Result<()> {
    replace_with(&ReplaceOps::default(), src, dst)
}

/// ###27-B2 — the rename_replace core over injectable ops. Windows arms
/// retry transient locks; the unix arm is single-shot. Retry exhaustion
/// returns the LAST observed error (not the first — the doc comment on
/// rename_replace says exactly that).
#[allow(clippy::too_many_lines)]
pub(crate) fn replace_with(ops: &ReplaceOps, src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        // #39 — both steps route through fs_retry (shared env-tunable
        // budget, OCTOS_FS_RETRY_TOTAL_MS; default 2.5s). The injectable
        // sleep/is_transient keep the ###29 mechanical pins meaningful.
        let is_t = &ops.is_transient;
        let remove_result = fs_retry(
            &ops.clock,
            ops.budget,
            || match (ops.remove)(dst) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()), // first cycle
                Err(err) => Err(err),
            },
            |e| is_t(e),
        );
        remove_result?;
        fs_retry(
            &ops.clock,
            ops.budget,
            || (ops.rename)(src, dst),
            |e| is_t(e),
        )
    }
    #[cfg(not(windows))]
    {
        match (ops.remove)(dst) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        (ops.rename)(src, dst)
    }
}

pub(super) const COALESCED_METADATA_KEYS: &[&str] = &[
    "coalesced_child_ids",
    "coalesced_children",
    "coalesced_count",
    "coalesced_count_kind",
    "listed_child_count",
    "omitted_child_count",
    "coalesced_row_kinds",
    "omitted_summary_count",
    "coalesce_generation",
    "coalesced_child_ids_truncated",
    "coalesced_children_truncated",
    "coalesced_correction_note",
];

/// Canonical scope is authoritative even when two workspaces share a display
/// path. Without a canonical key, only exact legacy path evidence is safe here;
/// peer-stamp decoding requires the roster evidence owned by WorkspaceCompat.
fn continuation_workspace(record: &PendingContinuationRecord) -> Option<&serde_json::Value> {
    record
        .metadata
        .get("payload:workspace_scope")
        .or_else(|| record.metadata.get("payload:workspace"))
}

fn merge_continuation(
    existing: PendingContinuationRecord,
    mut next: PendingContinuationRecord,
) -> PendingContinuationRecord {
    // Completed carriers have delivered or retired their reports. Carrying
    // those fields into a correction would replay already-resolved siblings.
    if existing.status != ContinuationStatus::Completed
        && existing.group_id == next.group_id
        && existing.metadata.get("session_id") == next.metadata.get("session_id")
        && existing.metadata.get("profile_id") == next.metadata.get("profile_id")
        && continuation_workspace(&existing) == continuation_workspace(&next)
    {
        for key in COALESCED_METADATA_KEYS {
            let key = format!("payload:{key}");
            if let Some(value) = existing.metadata.get(&key) {
                next.metadata.entry(key).or_insert_with(|| value.clone());
            }
        }
    }
    // #1707 round 3 follow-up: when a strictly-higher-attempt CORRECTION
    // resurrects the payload at a LOWER status rank (e.g. Queued replacing a
    // Completed tombstone), the tombstone's lifecycle timestamps
    // (started_at_ms / completed_at_ms) must NOT survive into the corrected
    // record — a Queued record with completion timestamps is a lie.
    let is_correction = continuation_rank(&next.status) < continuation_rank(&existing.status);
    if is_correction {
        next.started_at_ms = None;
        next.completed_at_ms = None;
    } else {
        next.started_at_ms = match (existing.started_at_ms, next.started_at_ms) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        next.completed_at_ms = match (existing.completed_at_ms, next.completed_at_ms) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
    }
    if next.result.is_none() {
        next.result = existing.result;
    }
    next
}

fn continuation_rank(status: &ContinuationStatus) -> u8 {
    match status {
        ContinuationStatus::Queued => 0,
        ContinuationStatus::Started => 1,
        ContinuationStatus::Completed => 2,
    }
}

fn child_status_for_terminal(kind: &TerminalKind) -> ChildStatus {
    match kind {
        TerminalKind::Completed => ChildStatus::Completed,
        TerminalKind::Failed => ChildStatus::Failed,
        TerminalKind::Cancelled => ChildStatus::Cancelled,
    }
}

fn group_status_for_terminal(kind: &TerminalKind) -> GroupStatus {
    match kind {
        TerminalKind::Completed => GroupStatus::Completed,
        TerminalKind::Failed => GroupStatus::Failed,
        TerminalKind::Cancelled => GroupStatus::Cancelled,
    }
}

fn should_replace_terminal(existing: &Option<TerminalState>, next: &TerminalState) -> bool {
    existing
        .as_ref()
        .is_none_or(|current| next.finished_at_ms >= current.finished_at_ms)
}

fn is_auto_group_terminal(terminal: &Option<TerminalState>) -> bool {
    terminal
        .as_ref()
        .and_then(|terminal| terminal.message.as_deref())
        == Some(AUTO_GROUP_TERMINAL_MESSAGE)
}

fn push_unique(items: &mut Vec<String>, item: String) {
    if !items.iter().any(|existing| existing == &item) {
        items.push(item);
    }
}

fn child_key(group_id: &str, child_id: &str) -> String {
    format!("{group_id}/{child_id}")
}

fn artifact_key(group_id: &str, artifact_id: &str) -> String {
    format!("{group_id}/{artifact_id}")
}

fn continuation_key(group_id: &str, continuation_id: &str) -> String {
    format!("{group_id}/{continuation_id}")
}

/// fsync a directory so a just-renamed file's directory entry is durable
/// before dependent destructive steps (compaction) proceed. No-op on
/// non-Unix: std cannot open directories for syncing there, so on Windows the
/// rename ordering is only as durable as the filesystem makes it (the renames
/// themselves stay atomic; this only affects hard power-loss ordering).
fn fsync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn invalid_data(err: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}
