#![allow(dead_code)]
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
    },
    ContinuationCompleted {
        group_id: String,
        continuation_id: String,
        completed_at_ms: u64,
        #[serde(default)]
        result: Option<String>,
    },
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
        self.last_sequence = self.last_sequence.max(row.sequence);
        if !row.event_id.is_empty() && !self.applied_event_ids.insert(row.event_id.clone()) {
            return;
        }
        self.apply_event(&row.event, row.recorded_at_ms);
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
            } => self.apply_continuation_started(group_id, continuation_id, *started_at_ms),
            SupervisorEvent::ContinuationCompleted {
                group_id,
                continuation_id,
                completed_at_ms,
                result,
            } => self.apply_continuation_completed(
                group_id,
                continuation_id,
                *completed_at_ms,
                result.clone(),
            ),
        }
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
                if continuation_rank(&continuation.status) >= continuation_rank(&existing.status) {
                    *existing = merge_continuation(existing.clone(), continuation);
                }
            }
            None => {
                self.continuations.insert(key, continuation);
            }
        }
    }

    fn apply_continuation_started(
        &mut self,
        group_id: &str,
        continuation_id: &str,
        started_at_ms: u64,
    ) {
        self.ensure_group(group_id, started_at_ms);
        let key = continuation_key(group_id, continuation_id);
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
    ) {
        self.ensure_group(group_id, completed_at_ms);
        let key = continuation_key(group_id, continuation_id);
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
    /// `Some(n)`: fsync the events file every n-th `append_event`;
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

    /// Override the auto-snapshot cadence (live-ledger rows that trigger
    /// snapshot + compaction on append). `0` disables auto-compaction.
    pub fn with_snapshot_every_appends(mut self, every: u64) -> Self {
        self.snapshot_every_appends = every;
        self
    }

    /// Opt into batched append durability: fsync the events file every
    /// `every`-th `append_event`. `0` keeps the default (no append fsync —
    /// the documented trade-off on `append_ledger_row`). Snapshots are always
    /// fsynced regardless of this setting.
    pub fn with_append_fsync_every(mut self, every: u64) -> Self {
        self.append_fsync_every = (every > 0).then_some(every);
        self
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    pub fn events_path(&self) -> &Path {
        &self.events_path
    }

    /// Previous ledger generation rotated aside by the last compaction (kept
    /// for forensics; replaced on each compaction, never replayed).
    pub fn rotated_events_path(&self) -> &Path {
        &self.rotated_events_path
    }

    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot_path
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
        Ok(state)
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

    /// Write a durable snapshot of the current state WITHOUT compacting the
    /// ledger (tmp + fsync + atomic rename + dir fsync). Takes the append
    /// lock so it cannot race a compaction in another process. This is also
    /// exactly the first half of a compaction cycle, so tests use it to
    /// simulate a crash between "snapshot written" and "ledger rotated" —
    /// `load_state` replays that layout idempotently.
    pub fn write_snapshot(&self) -> io::Result<SupervisorSnapshot> {
        let _lock = self.acquire_append_lock()?;
        self.write_snapshot_locked()
    }

    /// Snapshot the current state and compact the ledger: rows covered by the
    /// snapshot rotate to `supervisor-events.jsonl.old` (one generation kept
    /// for forensics). Intended for shutdown paths and maintenance; appends
    /// invoke the same cycle automatically every [`SNAPSHOT_EVERY_APPENDS`]
    /// ledger rows.
    ///
    /// Crash-safe ordering: the snapshot is durable (file + dir fsync) before
    /// the ledger is rotated. A crash in between leaves snapshot + full
    /// ledger, which `load_state` replays idempotently (rows at or below
    /// `snapshot.last_sequence` are skipped).
    pub fn snapshot_now(&self) -> io::Result<SupervisorSnapshot> {
        let _lock = self.acquire_append_lock()?;
        let mut cache = self.lock_seq_cache();
        self.snapshot_and_compact_locked(&mut cache)
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
        self.refresh_seq_cache_locked(&mut cache)?;
        let sequence = cache.last_sequence.saturating_add(1);
        let mut event_id = event_id.into();
        if event_id.is_empty() {
            event_id = format!("event:{sequence}");
        }
        let row = SupervisorEventLedgerRow {
            event_id,
            sequence,
            recorded_at_ms: unix_time_millis(),
            event,
        };
        self.append_row_locked(&row, &mut cache)?;
        if self.snapshot_every_appends > 0 && cache.ledger_rows >= self.snapshot_every_appends {
            if let Err(err) = self.snapshot_and_compact_locked(&mut cache) {
                tracing::warn!(
                    error = %err,
                    events_path = %self.events_path.display(),
                    "supervisor ledger auto-compaction failed; will retry on a later append"
                );
            }
        }
        Ok(row)
    }

    /// Append one raw row to the event ledger, bypassing sequence assignment
    /// (callers own the sequence; used by tests and repair tooling). Takes
    /// the same append file-lock as every other events-file writer — an
    /// unlocked write could race a concurrent compaction and be rotated away
    /// unreplayed (data loss despite `Ok`). Raw sequences MUST be above the
    /// current maximum (and above any snapshot's `last_sequence`): replay
    /// skips rows at or below the snapshot cutoff, so a back-dated raw row
    /// would be ignored by `load_state`. Raw appends do not trigger
    /// auto-compaction; the next `append_event` does, after reseeding.
    ///
    /// The write reaches the OS in a single unbuffered `write_all` (the
    /// trailing `flush()` is a no-op; there is no user-space buffer to
    /// drain).
    ///
    /// DURABILITY (KNOWN LIMITATION, accepted): by default the append is
    /// handed to the OS but is NOT `fsync`-ed, so the OS page cache may
    /// briefly hold the last appended row(s) before the disk physically
    /// commits. An ordinary process crash (panic / kill / OOM) is safe — the
    /// OS still flushes its cache — but a HARD power loss or kernel panic in
    /// that window can lose the most recent append. This is a STORE-WIDE
    /// property: every group / terminal / continuation record rides this
    /// path, not just peer continuations, so an unconditional `fsync` here
    /// would be a store-wide latency cost. Under the best-effort
    /// peer-delivery model (see `peer_send_input_authorized`) a peer
    /// injection lost only to a simultaneous power cut is within the
    /// documented semantics. Callers that need bounded power-loss exposure
    /// can opt into batched fsync via `with_append_fsync_every(n)` (applies
    /// to `append_event`); snapshots are always fsynced (file + directory)
    /// because compaction deletes the ledger rows they replace.
    pub fn append_ledger_row(&self, row: &SupervisorEventLedgerRow) -> io::Result<()> {
        let _lock = self.acquire_append_lock()?;
        let mut cache = self.lock_seq_cache();
        self.write_row_sealed_locked(row)?;
        // Raw rows bypass sequence assignment; rather than guess at the
        // ledger's shape (this path must keep working even on a ledger whose
        // other rows are unparseable), drop the cursor and let the next
        // sequenced append reseed from disk.
        cache.seeded = false;
        Ok(())
    }

    pub fn record_group_registered(
        &self,
        group: SupervisedGroupRecord,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let event_id = format!("group_registered:{}", group.group_id);
        self.append_event(event_id, SupervisorEvent::GroupRegistered { group })
    }

    pub fn record_group_terminal(
        &self,
        group_id: impl Into<String>,
        terminal: TerminalState,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let group_id = group_id.into();
        let event_id = format!(
            "group_terminal:{group_id}:{:?}:{}",
            terminal.kind, terminal.finished_at_ms
        );
        self.append_event(
            event_id,
            SupervisorEvent::GroupTerminal { group_id, terminal },
        )
    }

    pub fn record_child_started(
        &self,
        child: ChildAgentRecord,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let event_id = format!("child_started:{}:{}", child.group_id, child.child_id);
        self.append_event(event_id, SupervisorEvent::ChildStarted { child })
    }

    pub fn record_heartbeat(&self, ping: HeartbeatPing) -> io::Result<SupervisorEventLedgerRow> {
        let ping_part = ping
            .ping_id
            .as_deref()
            .map_or_else(|| ping.observed_at_ms.to_string(), ToString::to_string);
        let event_id = format!("heartbeat:{}:{}:{ping_part}", ping.group_id, ping.child_id);
        self.append_event(event_id, SupervisorEvent::Heartbeat { ping })
    }

    pub fn record_child_completed(
        &self,
        group_id: impl Into<String>,
        child_id: impl Into<String>,
        finished_at_ms: u64,
        message: Option<String>,
    ) -> io::Result<SupervisorEventLedgerRow> {
        self.record_child_terminal(
            group_id,
            child_id,
            TerminalState::completed(finished_at_ms, message),
        )
    }

    pub fn record_child_failed(
        &self,
        group_id: impl Into<String>,
        child_id: impl Into<String>,
        finished_at_ms: u64,
        exit_code: Option<i32>,
        reason: Option<String>,
    ) -> io::Result<SupervisorEventLedgerRow> {
        self.record_child_terminal(
            group_id,
            child_id,
            TerminalState::failed(finished_at_ms, exit_code, reason),
        )
    }

    pub fn record_child_cancelled(
        &self,
        group_id: impl Into<String>,
        child_id: impl Into<String>,
        finished_at_ms: u64,
        reason: Option<String>,
    ) -> io::Result<SupervisorEventLedgerRow> {
        self.record_child_terminal(
            group_id,
            child_id,
            TerminalState::cancelled(finished_at_ms, reason),
        )
    }

    pub fn record_child_terminal(
        &self,
        group_id: impl Into<String>,
        child_id: impl Into<String>,
        terminal: TerminalState,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let group_id = group_id.into();
        let child_id = child_id.into();
        let event_id = format!(
            "child_terminal:{group_id}:{child_id}:{:?}:{}",
            terminal.kind, terminal.finished_at_ms
        );
        self.append_event(
            event_id,
            SupervisorEvent::ChildTerminal {
                group_id,
                child_id,
                terminal,
            },
        )
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

    pub fn record_continuation_started(
        &self,
        group_id: impl Into<String>,
        continuation_id: impl Into<String>,
        started_at_ms: u64,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let group_id = group_id.into();
        let continuation_id = continuation_id.into();
        let event_id = format!("continuation_started:{group_id}:{continuation_id}:{started_at_ms}");
        self.append_event(
            event_id,
            SupervisorEvent::ContinuationStarted {
                group_id,
                continuation_id,
                started_at_ms,
            },
        )
    }

    pub fn record_continuation_completed(
        &self,
        group_id: impl Into<String>,
        continuation_id: impl Into<String>,
        completed_at_ms: u64,
        result: Option<String>,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let group_id = group_id.into();
        let continuation_id = continuation_id.into();
        let event_id =
            format!("continuation_completed:{group_id}:{continuation_id}:{completed_at_ms}");
        self.append_event(
            event_id,
            SupervisorEvent::ContinuationCompleted {
                group_id,
                continuation_id,
                completed_at_ms,
                result,
            },
        )
    }

    /// Whole-line append (append lock must be held — EVERY events-file write
    /// goes through here under the lock; an unlocked write could land between
    /// a concurrent compaction's "snapshot rows" and "rotate ledger" steps
    /// and be rotated away unreplayed). Returns the handle so callers can
    /// fsync it.
    ///
    /// Two repairs happen inline:
    /// - Torn tail: if the file is non-empty and does not end in a newline
    ///   (a crash split an earlier append), a `\n` is written first so this
    ///   row can never concatenate onto the torn content. The torn bytes are
    ///   preserved, never truncated — they may be a complete row that merely
    ///   lost its terminator.
    /// - Fresh file: when this write CREATES the ledger (fresh store, or the
    ///   first append after a compaction rotated it away), the parent
    ///   directory is fsynced so the new name is durable — without this the
    ///   batched append-fsync bound would be void (power loss could keep the
    ///   snapshot but lose the new ledger's directory entry).
    ///
    /// The row and its terminator go down in a single `write_all`, keeping
    /// the torn-write exposure to one syscall.
    fn write_row_sealed_locked(&self, row: &SupervisorEventLedgerRow) -> io::Result<File> {
        self.ensure_root_dir()?;
        let created = !self.events_path.exists();
        let mut file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(&self.events_path)?;
        let len = file.metadata()?.len();
        let mut payload = Vec::with_capacity(256);
        if len > 0 {
            file.seek(SeekFrom::Start(len - 1))?;
            let mut last = [0_u8; 1];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                payload.push(b'\n');
            }
        }
        serde_json::to_writer(&mut payload, row).map_err(invalid_data)?;
        payload.push(b'\n');
        // O_APPEND: the write lands at EOF regardless of the read seek above.
        file.write_all(&payload)?;
        file.flush()?;
        if created {
            fsync_dir(&self.root_dir)?;
        }
        Ok(file)
    }

    /// Append a sequenced row while holding the append lock: write, apply
    /// the batched fsync policy, and advance the in-memory cursor to the new
    /// tail.
    fn append_row_locked(
        &self,
        row: &SupervisorEventLedgerRow,
        cache: &mut SeqCache,
    ) -> io::Result<()> {
        let file = self.write_row_sealed_locked(row)?;
        if let Some(every) = self.append_fsync_every {
            cache.appends_since_fsync = cache.appends_since_fsync.saturating_add(1);
            if cache.appends_since_fsync >= every {
                file.sync_data()?;
                cache.appends_since_fsync = 0;
            }
        }
        // #34f — release the events-file handle BEFORE the caller may
        // compact: on Windows, renaming a file that still has an open handle
        // fails, and `append_event`'s auto-compaction below renames exactly
        // this file. POSIX tolerates the open handle, which is why the five
        // snapshot tests were green on Linux and Os error 3 on Windows.
        cache.last_sequence = cache.last_sequence.max(row.sequence);
        // Exact length of the file we just extended; nothing else can write
        // while we hold the lock.
        cache.events_len = file.metadata()?.len();
        // #34f — release the events-file handle BEFORE the caller may
        // compact (metadata read above must come first): on Windows,
        // renaming a file that still has an open handle fails, and
        // `append_event`'s auto-compaction renames exactly this file.
        // POSIX tolerates the open handle — the five snapshot tests were
        // green on Linux and Os error 3 on Windows for exactly this reason.
        drop(file);
        cache.ledger_rows = cache.ledger_rows.saturating_add(1);
        cache.seeded = true;
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
        {
            let mut tmp = File::create(&tmp_path)?;
            tmp.write_all(body.as_bytes())?;
            tmp.sync_all()?;
        }
        fs::rename(&tmp_path, &self.snapshot_path)?;
        fsync_dir(&self.root_dir)?;
        Ok(snapshot)
    }

    /// Second half of the compaction cycle (append lock must be held): once
    /// the snapshot is durable, rotate the fully-applied ledger to the single
    /// `.jsonl.old` forensics generation and reset the cursor. On any partial
    /// failure the cursor is left stale, which the next append detects and
    /// repairs by reseeding from disk.
    fn snapshot_and_compact_locked(&self, cache: &mut SeqCache) -> io::Result<SupervisorSnapshot> {
        let snapshot = self.write_snapshot_locked()?;
        if self.events_path.exists() {
            match fs::remove_file(&self.rotated_events_path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            // #34f — Windows rename is NOT replace-on-existing; the remove
            // above cleared the target, so a plain rename works on both
            // platforms. (kept explicit for the audit trail)
            fs::rename(&self.events_path, &self.rotated_events_path)?;
            fsync_dir(&self.root_dir)?;
        }
        cache.seeded = true;
        cache.last_sequence = cache.last_sequence.max(snapshot.last_sequence);
        cache.events_len = 0;
        cache.ledger_rows = 0;
        // The snapshot fsync covered everything the ledger held.
        cache.appends_since_fsync = 0;
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
                guard
            }
        }
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
        // Open-and-match instead of exists()-then-open: a concurrent
        // compaction may rotate the ledger away between the two, which must
        // read as "no rows", not an error.
        let file = match File::open(&self.events_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
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
                Ok(row) => rows.push(row),
                Err(err) => {
                    skipped += 1;
                    tracing::warn!(
                        target: "octos::supervisor",
                        path = %self.events_path.display(),
                        line = idx + 1,
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
        fs::create_dir_all(&self.root_dir)
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

fn merge_continuation(
    existing: PendingContinuationRecord,
    mut next: PendingContinuationRecord,
) -> PendingContinuationRecord {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "octos-supervisor-store-{label}-{}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.path.parent().unwrap());
        }
    }

    /// #26a — tolerant replay: a malformed row (torn write, unknown variant
    /// from an older/newer schema) must be SKIPPED with a warn, never abort
    /// the whole stream load. Live evidence: the a9c4 instance stream carried
    /// 3 legacy `goal_state` rows and `octos goal list` was unusable before.
    #[test]
    fn malformed_rows_are_skipped_not_fatal() {
        let dir = TestDir::new("tolerant");
        let store = SupervisorStore::new(&dir.path);

        let mut group = SupervisedGroupRecord::new("group-1", 100);
        group.objective = Some("tolerant replay".to_string());
        group.metadata.insert(
            "autonomy_record_kind".to_string(),
            serde_json::json!("goal"),
        );
        group
            .metadata
            .insert("goal_id".to_string(), serde_json::json!("goal_01"));
        group
            .metadata
            .insert("status".to_string(), serde_json::json!("active"));
        store.record_group_registered(group).unwrap();

        // Corrupt the stream: one unknown-variant row and one torn JSON line.
        let mut raw = std::fs::read_to_string(&store.events_path).unwrap();
        raw.push_str("{\"event\":{\"type\":\"goal_state\",\"payload\":{}}}\n");
        raw.push_str("{\"torn json…\n");
        std::fs::write(&store.events_path, raw).unwrap();

        // The good row still loads (malformed rows skipped, not fatal).
        let state = store.load_state().unwrap();
        assert!(
            state.groups.contains_key("group-1"),
            "the well-formed row must survive malformed siblings"
        );
        // The goal-scoped view survives too (no session_id metadata in this
        // fixture, so the composite key degenerates to "\u{1}goal_01").
        let by_goal = store.load_goal_groups_by_id().unwrap();
        assert_eq!(
            by_goal
                .get("\u{1}goal_01")
                .map(|g| g.metadata.get("status")),
            Some(Some(&serde_json::json!("active")))
        );
    }

    /// #26a — the goal-scoped view folds BY GOAL ID, so a superseded goal
    /// (an earlier goal_NN sharing the newest goal's session-scope group)
    /// stays visible for zombie cleanup where the group-folded `load_state`
    /// map would only hold the newest one.
    #[test]
    fn goal_scoped_view_keeps_superseded_goals_visible() {
        let dir = TestDir::new("goal-scoped");
        let store = SupervisorStore::new(&dir.path);

        let goal_row = |goal_id: &str, status: &str, seq: u64| {
            let mut group = SupervisedGroupRecord::new("autonomy-goal:scope-1", seq);
            group.objective = Some(format!("objective {goal_id}"));
            group.metadata.insert(
                "autonomy_record_kind".to_string(),
                serde_json::json!("goal"),
            );
            group
                .metadata
                .insert("goal_id".to_string(), serde_json::json!(goal_id));
            group
                .metadata
                .insert("profile_id".to_string(), serde_json::json!("octos"));
            group.metadata.insert(
                "session_id".to_string(),
                serde_json::json!("octos:local:tui#coding"),
            );
            group
                .metadata
                .insert("status".to_string(), serde_json::json!(status));
            group
        };
        // Three goals of the SAME scope group, newest last.
        store
            .record_group_registered(goal_row("goal_01", "active", 1))
            .unwrap();
        store
            .record_group_registered(goal_row("goal_02", "complete", 2))
            .unwrap();
        store
            .record_group_registered(goal_row("goal_03", "complete", 3))
            .unwrap();

        // The group-folded state collapses re-registrations of the SAME
        // group id (the `group_registered:<group_id>` event id dedupes), so
        // replay may hold whichever registration survived — exactly why the
        // goal-scoped view exists. We only assert the group exists here.
        let state = store.load_state().unwrap();
        assert!(
            state.groups.contains_key("autonomy-goal:scope-1"),
            "the scope group exists in the folded state"
        );

        // The goal-scoped view keeps ALL THREE visible, each with its own
        // latest status — the zombie-cleanup requirement. Keys are the
        // composite (session, goal) since #26a-r1; the three goals share
        // one session, so the count and statuses are unchanged.
        let session = "octos:local:tui#coding";
        let sep = char::from_u32(1).expect("unit separator");
        let key = |goal_id: &str| format!("{session}{sep}{goal_id}");
        let by_goal = store.load_goal_groups_by_id().unwrap();
        assert_eq!(by_goal.len(), 3, "all goals stay visible");
        assert_eq!(
            by_goal
                .get(&key("goal_01"))
                .and_then(|g| g.metadata.get("status")),
            Some(&serde_json::json!("active"))
        );
        assert_eq!(
            by_goal
                .get(&key("goal_02"))
                .and_then(|g| g.metadata.get("status")),
            Some(&serde_json::json!("complete"))
        );
        assert_eq!(
            by_goal
                .get(&key("goal_03"))
                .and_then(|g| g.metadata.get("status")),
            Some(&serde_json::json!("complete"))
        );
    }

    #[test]
    fn appends_replays_and_snapshots_supervisor_lifecycle() {
        let dir = TestDir::new("lifecycle");
        let store = SupervisorStore::new(&dir.path);

        let mut group = SupervisedGroupRecord::new("group-1", 100);
        group.objective = Some("ship durable supervisors".to_string());
        store.record_group_registered(group).unwrap();

        let mut child = ChildAgentRecord::new("group-1", "child-a", 110);
        child.label = Some("Worker Ada".to_string());
        child.task = Some("implement persistence".to_string());
        store.record_child_started(child).unwrap();

        store
            .record_heartbeat(HeartbeatPing {
                group_id: "group-1".to_string(),
                child_id: "child-a".to_string(),
                ping_id: Some("ping-1".to_string()),
                observed_at_ms: 120,
                state: Some("running".to_string()),
                message: Some("writing tests".to_string()),
                progress_percent: Some(40),
                metadata: SupervisorMetadata::new(),
            })
            .unwrap();

        store
            .record_artifact_updated(ArtifactRecord {
                group_id: "group-1".to_string(),
                child_id: Some("child-a".to_string()),
                artifact_id: "patch".to_string(),
                kind: "file".to_string(),
                path: "crates/octos-cli/src/api/supervisor_store.rs".to_string(),
                display_name: None,
                version: 1,
                updated_at_ms: 130,
                sha256: None,
                bytes: Some(4096),
                metadata: SupervisorMetadata::new(),
            })
            .unwrap();

        store
            .record_continuation_queued(PendingContinuationRecord {
                group_id: "group-1".to_string(),
                continuation_id: "cont-1".to_string(),
                child_id: Some("child-a".to_string()),
                prompt: Some("continue after restart".to_string()),
                status: ContinuationStatus::Queued,
                queued_at_ms: 140,
                started_at_ms: None,
                completed_at_ms: None,
                result: None,
                attempt: 1,
                metadata: SupervisorMetadata::new(),
            })
            .unwrap();
        store
            .record_continuation_started("group-1", "cont-1", 150)
            .unwrap();
        store
            .record_continuation_completed("group-1", "cont-1", 160, Some("resumed".to_string()))
            .unwrap();
        store
            .record_child_completed("group-1", "child-a", 170, Some("done".to_string()))
            .unwrap();

        let state = store.load_state().unwrap();
        assert_eq!(state.groups["group-1"].status, GroupStatus::Completed);
        assert_eq!(
            state.children[&child_key("group-1", "child-a")].status,
            ChildStatus::Completed
        );
        assert_eq!(
            state.artifacts[&artifact_key("group-1", "patch")].bytes,
            Some(4096)
        );
        assert_eq!(
            state.continuations[&continuation_key("group-1", "cont-1")].status,
            ContinuationStatus::Completed
        );

        let snapshot = store.write_snapshot().unwrap();
        assert_eq!(snapshot.last_sequence, state.last_sequence);

        let restored = SupervisorStore::new(&dir.path).load_state().unwrap();
        assert_eq!(restored.groups["group-1"].status, GroupStatus::Completed);
        assert_eq!(restored.last_sequence, state.last_sequence);
    }

    #[test]
    fn replay_tolerates_duplicate_event_ids_and_keeps_latest_records() {
        let dir = TestDir::new("duplicates");
        let store = SupervisorStore::new(&dir.path);

        let stale_heartbeat = SupervisorEventLedgerRow {
            event_id: "heartbeat:dup".to_string(),
            sequence: 1,
            recorded_at_ms: 10,
            event: SupervisorEvent::Heartbeat {
                ping: HeartbeatPing {
                    group_id: "group-2".to_string(),
                    child_id: "child-b".to_string(),
                    ping_id: Some("dup".to_string()),
                    observed_at_ms: 20,
                    state: Some("running".to_string()),
                    message: Some("old".to_string()),
                    progress_percent: Some(10),
                    metadata: SupervisorMetadata::new(),
                },
            },
        };
        store.append_ledger_row(&stale_heartbeat).unwrap();
        store.append_ledger_row(&stale_heartbeat).unwrap();

        store
            .record_heartbeat(HeartbeatPing {
                group_id: "group-2".to_string(),
                child_id: "child-b".to_string(),
                ping_id: Some("fresh".to_string()),
                observed_at_ms: 30,
                state: Some("running".to_string()),
                message: Some("new".to_string()),
                progress_percent: Some(80),
                metadata: SupervisorMetadata::new(),
            })
            .unwrap();

        store
            .record_artifact_updated(ArtifactRecord {
                group_id: "group-2".to_string(),
                child_id: Some("child-b".to_string()),
                artifact_id: "report".to_string(),
                kind: "markdown".to_string(),
                path: "old.md".to_string(),
                display_name: None,
                version: 1,
                updated_at_ms: 40,
                sha256: None,
                bytes: Some(10),
                metadata: SupervisorMetadata::new(),
            })
            .unwrap();
        store
            .record_artifact_updated(ArtifactRecord {
                group_id: "group-2".to_string(),
                child_id: Some("child-b".to_string()),
                artifact_id: "report".to_string(),
                kind: "markdown".to_string(),
                path: "new.md".to_string(),
                display_name: None,
                version: 2,
                updated_at_ms: 50,
                sha256: None,
                bytes: Some(20),
                metadata: SupervisorMetadata::new(),
            })
            .unwrap();

        let state = store.load_state().unwrap();
        let child = &state.children[&child_key("group-2", "child-b")];
        assert_eq!(
            child
                .last_heartbeat
                .as_ref()
                .and_then(|p| p.progress_percent),
            Some(80)
        );
        assert_eq!(
            state.artifacts[&artifact_key("group-2", "report")].path,
            "new.md"
        );
        assert_eq!(state.applied_event_ids.len(), 4);
    }

    // Not run on Windows for the same reason as
    // `contending_stores_never_lose_raw_appends_to_compaction` below, and the
    // failure here is the WIDER half of #1999: this test does no compaction at
    // all — it is 16 threads doing plain concurrent `append_event` — and it
    // still fails with `Os { code: 5, PermissionDenied, "Access is denied." }`.
    // So the Windows limitation is CONCURRENT WRITERS generally (the ledger /
    // lock file cannot be opened by a second writer), not just rename-based
    // rotation. Single-writer use is unaffected.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn append_event_assigns_unique_monotonic_sequences_under_concurrency() {
        let dir = TestDir::new("concurrent-append");
        let store = Arc::new(SupervisorStore::new(&dir.path));
        let barrier = Arc::new(Barrier::new(16));
        let mut handles = Vec::new();

        for idx in 0..16_u64 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store
                    .record_heartbeat(HeartbeatPing {
                        group_id: "group-concurrent".to_string(),
                        child_id: format!("child-{idx}"),
                        ping_id: Some(format!("ping-{idx}")),
                        observed_at_ms: 1_000 + idx,
                        state: Some("running".to_string()),
                        message: None,
                        progress_percent: None,
                        metadata: SupervisorMetadata::new(),
                    })
                    .unwrap()
            }));
        }

        let mut rows = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| row.sequence);

        let sequences = rows.iter().map(|row| row.sequence).collect::<Vec<_>>();
        assert_eq!(sequences, (1..=16).collect::<Vec<_>>());

        let state = store.load_state().unwrap();
        assert_eq!(state.last_sequence, 16);
        assert_eq!(state.children.len(), 16);
    }

    #[test]
    fn auto_group_terminal_recomputes_when_late_child_is_observed() {
        let mut state = SupervisorState::default();
        state.apply_event(
            &SupervisorEvent::ChildStarted {
                child: ChildAgentRecord::new("group-rollup", "child-a", 100),
            },
            100,
        );
        state.apply_event(
            &SupervisorEvent::ChildTerminal {
                group_id: "group-rollup".to_string(),
                child_id: "child-a".to_string(),
                terminal: TerminalState::completed(150, Some("done".to_string())),
            },
            150,
        );
        assert_eq!(state.groups["group-rollup"].status, GroupStatus::Completed);

        state.apply_event(
            &SupervisorEvent::ChildStarted {
                child: ChildAgentRecord::new("group-rollup", "child-b", 200),
            },
            200,
        );
        assert_eq!(state.groups["group-rollup"].status, GroupStatus::Running);
        assert_eq!(state.groups["group-rollup"].terminal, None);

        state.apply_event(
            &SupervisorEvent::ChildTerminal {
                group_id: "group-rollup".to_string(),
                child_id: "child-b".to_string(),
                terminal: TerminalState::failed(300, Some(1), Some("failed".to_string())),
            },
            300,
        );

        let group = &state.groups["group-rollup"];
        assert_eq!(group.status, GroupStatus::Failed);
        assert_eq!(group.terminal.as_ref().unwrap().kind, TerminalKind::Failed);
        assert_eq!(group.terminal.as_ref().unwrap().finished_at_ms, 300);
    }

    #[test]
    fn serde_round_trips_public_records() {
        let terminal = TerminalState::failed(250, Some(2), Some("validator failed".to_string()));
        let json = serde_json::to_string(&terminal).unwrap();
        let restored: TerminalState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.kind, TerminalKind::Failed);
        assert_eq!(restored.exit_code, Some(2));
    }

    // ---- #1974 scaling: cached sequence, production snapshots, compaction ----

    fn test_ping(group: &str, child: &str, ping_id: &str, observed_at_ms: u64) -> HeartbeatPing {
        HeartbeatPing {
            group_id: group.to_string(),
            child_id: child.to_string(),
            ping_id: Some(ping_id.to_string()),
            observed_at_ms,
            state: Some("running".to_string()),
            message: None,
            progress_percent: None,
            metadata: SupervisorMetadata::new(),
        }
    }

    /// Reference state: full replay of every row ever appended, in order.
    /// Snapshot + compaction must reproduce this exactly.
    fn shadow_state(rows: &[SupervisorEventLedgerRow]) -> SupervisorState {
        let mut state = SupervisorState::default();
        for row in rows {
            state.apply_ledger_row(row);
        }
        state
    }

    fn live_ledger_rows(store: &SupervisorStore) -> Vec<SupervisorEventLedgerRow> {
        store.read_ledger_rows().unwrap()
    }

    fn rows_at(path: &Path) -> Vec<SupervisorEventLedgerRow> {
        let body = fs::read_to_string(path).unwrap();
        body.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn legacy_jsonl_only_ledger_loads_identically_and_load_is_read_only() {
        let dir = TestDir::new("legacy");
        let store = SupervisorStore::new(&dir.path);

        // Build a legacy dir the way old builds did: JSONL rows only, no
        // snapshot file anywhere.
        let mut rows = Vec::new();
        for idx in 1..=5_u64 {
            let row = SupervisorEventLedgerRow {
                event_id: format!("heartbeat:legacy:{idx}"),
                sequence: idx,
                recorded_at_ms: 1_000 + idx,
                event: SupervisorEvent::Heartbeat {
                    ping: test_ping(
                        "group-legacy",
                        &format!("child-{idx}"),
                        &format!("ping-{idx}"),
                        1_000 + idx,
                    ),
                },
            };
            store.append_ledger_row(&row).unwrap();
            rows.push(row);
        }
        assert!(!store.snapshot_path().exists());

        let fresh = SupervisorStore::new(&dir.path);
        let state = fresh.load_state().unwrap();
        assert_eq!(state, shadow_state(&rows));
        assert_eq!(state.last_sequence, 5);
        // Loading must never write: no snapshot, no rotation, ledger intact.
        assert!(!fresh.snapshot_path().exists());
        assert!(!fresh.rotated_events_path().exists());
        assert_eq!(live_ledger_rows(&fresh).len(), 5);
    }

    #[test]
    fn append_auto_snapshots_and_compacts_after_threshold() {
        let dir = TestDir::new("auto-compact");
        let store = SupervisorStore::new(&dir.path).with_snapshot_every_appends(8);

        let mut rows = Vec::new();
        for idx in 1..=20_u64 {
            rows.push(
                store
                    .record_heartbeat(test_ping(
                        "group-auto",
                        &format!("child-{idx}"),
                        &format!("ping-{idx}"),
                        1_000 + idx,
                    ))
                    .unwrap(),
            );
        }
        let sequences: Vec<u64> = rows.iter().map(|row| row.sequence).collect();
        assert_eq!(sequences, (1..=20).collect::<Vec<_>>());

        // Two compaction cycles happened (after rows 8 and 16): snapshot
        // present, one rotated generation kept, live ledger holds the tail.
        assert!(store.snapshot_path().exists());
        assert!(store.rotated_events_path().exists());
        let tail = live_ledger_rows(&store);
        assert!(
            tail.len() < 8,
            "live ledger should be compacted, got {} rows",
            tail.len()
        );
        assert_eq!(tail.first().map(|row| row.sequence), Some(17));

        let snapshot = store.load_snapshot().unwrap().unwrap();
        assert_eq!(snapshot.last_sequence, 16);

        // Exactly one .old generation: the most recent rotated prefix.
        let rotated = rows_at(store.rotated_events_path());
        assert_eq!(rotated.first().map(|row| row.sequence), Some(9));
        assert_eq!(rotated.last().map(|row| row.sequence), Some(16));

        // Deep equality — including `applied_event_ids`: snapshot + tail
        // replays to the same state as a full replay of every event ever
        // appended. The id set is retained in full across snapshots (the
        // durable dedup contract; see `write_snapshot_locked` for the
        // documented growth residual).
        let restored = SupervisorStore::new(&dir.path).load_state().unwrap();
        assert_eq!(restored, shadow_state(&rows));
        assert_eq!(restored.last_sequence, 20);
        assert_eq!(restored.applied_event_ids.len(), 20);
    }

    #[test]
    fn default_snapshot_cadence_compacts_at_snapshot_every_appends() {
        let dir = TestDir::new("default-cadence");
        let store = SupervisorStore::new(&dir.path);
        for idx in 1..=(SNAPSHOT_EVERY_APPENDS + 1) {
            store
                .record_heartbeat(test_ping(
                    "group-cadence",
                    "child-a",
                    &format!("ping-{idx}"),
                    idx,
                ))
                .unwrap();
            if idx == SNAPSHOT_EVERY_APPENDS - 1 {
                assert!(
                    !store.snapshot_path().exists(),
                    "must not snapshot before the cadence threshold"
                );
            }
        }
        assert!(store.snapshot_path().exists());
        let snapshot = store.load_snapshot().unwrap().unwrap();
        assert_eq!(snapshot.last_sequence, SNAPSHOT_EVERY_APPENDS);
        assert_eq!(live_ledger_rows(&store).len(), 1);
        assert_eq!(
            store.load_state().unwrap().last_sequence,
            SNAPSHOT_EVERY_APPENDS + 1
        );
    }

    #[test]
    fn sequences_stay_monotonic_across_writers_with_independent_caches() {
        let dir = TestDir::new("cross-process");
        // Two store instances with independent seq caches simulate two
        // processes appending to the same ledger (the file lock is the only
        // coordination between them).
        let writer_a = SupervisorStore::new(&dir.path);
        let writer_b = SupervisorStore::new(&dir.path);

        // A seeds its cache with two appends (vec! evaluates in order).
        let mut rows = vec![
            writer_a
                .record_heartbeat(test_ping("g", "a", "a-1", 1))
                .unwrap(),
            writer_a
                .record_heartbeat(test_ping("g", "a", "a-2", 2))
                .unwrap(),
        ];
        // B appends behind A's back (A's cache is now stale).
        rows.push(
            writer_b
                .record_heartbeat(test_ping("g", "b", "b-1", 3))
                .unwrap(),
        );
        rows.push(
            writer_b
                .record_heartbeat(test_ping("g", "b", "b-2", 4))
                .unwrap(),
        );
        // A must detect the foreign growth and continue after B.
        rows.push(
            writer_a
                .record_heartbeat(test_ping("g", "a", "a-3", 5))
                .unwrap(),
        );
        // Raw out-of-band row (manual repair path) jumps the sequence forward;
        // later writers must continue past it, never clobber.
        let manual = SupervisorEventLedgerRow {
            event_id: "manual:100".to_string(),
            sequence: 100,
            recorded_at_ms: 6,
            event: SupervisorEvent::Heartbeat {
                ping: test_ping("g", "manual", "m-1", 6),
            },
        };
        writer_a.append_ledger_row(&manual).unwrap();
        rows.push(manual);
        rows.push(
            writer_b
                .record_heartbeat(test_ping("g", "b", "b-3", 7))
                .unwrap(),
        );

        let sequences: Vec<u64> = rows.iter().map(|row| row.sequence).collect();
        assert_eq!(sequences, vec![1, 2, 3, 4, 5, 100, 101]);
        // No clobbers on disk either.
        let on_disk: Vec<u64> = live_ledger_rows(&writer_a)
            .iter()
            .map(|row| row.sequence)
            .collect();
        assert_eq!(on_disk, sequences);
        assert_eq!(writer_b.load_state().unwrap(), shadow_state(&rows));
    }

    #[test]
    fn foreign_compaction_reseeds_stale_writer_and_sequences_continue() {
        let dir = TestDir::new("foreign-compact");
        let writer_a = SupervisorStore::new(&dir.path);
        let writer_b = SupervisorStore::new(&dir.path);

        let mut rows = Vec::new();
        for idx in 1..=3_u64 {
            rows.push(
                writer_a
                    .record_heartbeat(test_ping(
                        "g",
                        &format!("child-{idx}"),
                        &format!("p-{idx}"),
                        idx,
                    ))
                    .unwrap(),
            );
        }
        // Another process snapshots + compacts; A's cache still points at the
        // old fat ledger.
        let snapshot = writer_b.snapshot_now().unwrap();
        assert_eq!(snapshot.last_sequence, 3);
        assert!(live_ledger_rows(&writer_b).is_empty());

        let row = writer_a
            .record_heartbeat(test_ping("g", "child-4", "p-4", 4))
            .unwrap();
        assert_eq!(row.sequence, 4);
        rows.push(row);
        assert_eq!(writer_a.load_state().unwrap(), shadow_state(&rows));

        // Empty-ledger ABA: A compacts (its cache now says "empty ledger"),
        // then B appends AND compacts again — the ledger is back at the same
        // (zero) length but the snapshot moved. A must pick the sequence up
        // from the snapshot, not its stale cache.
        writer_a.snapshot_now().unwrap();
        rows.push(
            writer_b
                .record_heartbeat(test_ping("g", "child-5", "p-5", 5))
                .unwrap(),
        );
        assert_eq!(rows.last().unwrap().sequence, 5);
        writer_b.snapshot_now().unwrap();
        let row = writer_a
            .record_heartbeat(test_ping("g", "child-6", "p-6", 6))
            .unwrap();
        assert_eq!(row.sequence, 6);
        rows.push(row);
        assert_eq!(writer_a.load_state().unwrap(), shadow_state(&rows));
    }

    #[test]
    fn snapshot_without_compaction_is_idempotent_to_replay() {
        let dir = TestDir::new("crash-window");
        let store = SupervisorStore::new(&dir.path);
        let mut rows = Vec::new();
        for idx in 1..=5_u64 {
            rows.push(
                store
                    .record_heartbeat(test_ping(
                        "g",
                        &format!("c-{idx}"),
                        &format!("p-{idx}"),
                        idx,
                    ))
                    .unwrap(),
            );
        }
        // Simulate a crash between the two compaction halves: snapshot
        // written and durable, ledger NOT rotated (`write_snapshot` is
        // exactly that first half).
        let snapshot = store.write_snapshot().unwrap();
        assert_eq!(snapshot.last_sequence, 5);
        assert_eq!(live_ledger_rows(&store).len(), 5);

        // Snapshot + full (uncompacted) ledger must replay to the same state.
        assert_eq!(
            SupervisorStore::new(&dir.path).load_state().unwrap(),
            shadow_state(&rows)
        );

        // Appends after the interrupted compaction keep working…
        rows.push(
            store
                .record_heartbeat(test_ping("g", "c-6", "p-6", 6))
                .unwrap(),
        );
        assert_eq!(rows.last().unwrap().sequence, 6);
        assert_eq!(
            SupervisorStore::new(&dir.path).load_state().unwrap(),
            shadow_state(&rows)
        );

        // …and the next full cycle compacts the stale prefix away.
        store.snapshot_now().unwrap();
        assert!(live_ledger_rows(&store).is_empty());
        assert_eq!(
            SupervisorStore::new(&dir.path).load_state().unwrap(),
            shadow_state(&rows)
        );
    }

    #[test]
    fn snapshot_now_on_fresh_and_legacy_stores_is_safe() {
        let dir = TestDir::new("snapshot-now");
        let store = SupervisorStore::new(&dir.path);

        // Fresh dir: snapshot of the empty state, nothing to rotate.
        let snapshot = store.snapshot_now().unwrap();
        assert_eq!(snapshot.last_sequence, 0);
        assert!(!store.rotated_events_path().exists());
        assert_eq!(store.load_state().unwrap(), SupervisorState::default());

        // Legacy ledger (raw rows, no prior snapshot): snapshot_now compacts
        // it and preserves the state exactly.
        let mut rows = Vec::new();
        for idx in 1..=3_u64 {
            let row = SupervisorEventLedgerRow {
                event_id: format!("heartbeat:legacy:{idx}"),
                sequence: idx,
                recorded_at_ms: idx,
                event: SupervisorEvent::Heartbeat {
                    ping: test_ping("g", &format!("c-{idx}"), &format!("p-{idx}"), idx),
                },
            };
            store.append_ledger_row(&row).unwrap();
            rows.push(row);
        }
        let snapshot = store.snapshot_now().unwrap();
        assert_eq!(snapshot.last_sequence, 3);
        assert!(live_ledger_rows(&store).is_empty());
        assert!(store.rotated_events_path().exists());
        assert_eq!(store.load_state().unwrap(), shadow_state(&rows));

        // Sequences continue after an explicit snapshot.
        let row = store
            .record_heartbeat(test_ping("g", "c-4", "p-4", 4))
            .unwrap();
        assert_eq!(row.sequence, 4);
    }

    #[test]
    fn batched_append_fsync_mode_appends_and_loads() {
        let dir = TestDir::new("batched-fsync");
        // fsync effects are not observable through the fs API; this pins the
        // builder and that batched-fsync mode does not corrupt the ledger.
        let store = SupervisorStore::new(&dir.path).with_append_fsync_every(2);
        let mut rows = Vec::new();
        for idx in 1..=5_u64 {
            rows.push(
                store
                    .record_heartbeat(test_ping(
                        "g",
                        &format!("c-{idx}"),
                        &format!("p-{idx}"),
                        idx,
                    ))
                    .unwrap(),
            );
        }
        assert_eq!(store.load_state().unwrap(), shadow_state(&rows));
        assert_eq!(store.load_state().unwrap().last_sequence, 5);
    }

    // ---- #1974 codex round: locking, ABA, torn tail, schema guard ----

    // NOT run on Windows — and that gate documents a REAL product limitation,
    // not a test artifact. This test drives TWO independent writers at one dir
    // while compaction rotates the ledger by rename. Windows refuses to
    // rename/replace a file another handle still has open (sharing violation),
    // so thread A's `record_heartbeat` fails with `Os { code: 5,
    // PermissionDenied, "Access is denied." }` rather than losing rows. The
    // single-writer path (one `serve`, or one `octos chat --goals`) is
    // unaffected; genuine multi-writer supervisor-store compaction on Windows
    // needs retry-on-sharing-violation and is tracked separately.
    //
    // This surfaced only because the Phase 0 extraction (#1996) un-gated
    // `autonomy::*`, so these tests now run in the UNFEATURED build that
    // `check-windows` compiles — previously they were `api`-gated and never
    // ran there.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn contending_stores_never_lose_raw_appends_to_compaction() {
        let dir = TestDir::new("contend");
        // Two INDEPENDENT store instances (separate seq caches — a genuine
        // two-writer setup, unlike the shared-Arc concurrency test above)
        // hammer one dir from two threads in barrier-synchronized rounds:
        // each round, thread A appends a sequenced event and runs a full
        // snapshot_now compaction while thread B bursts raw
        // `append_ledger_row` writes straight into A's compaction window.
        // Pins the FIX-1 locking: an UNLOCKED raw append can land between
        // compaction's "read + snapshot the rows" and "rotate the ledger"
        // steps and be rotated away unreplayed — lost despite returning Ok.
        // (Verified: with the lock removed from `append_ledger_row`, this
        // test fails with lost raw children.)
        let store_a = SupervisorStore::new(&dir.path);
        let store_b = SupervisorStore::new(&dir.path);

        const ROUNDS: u64 = 24;
        const BURST: u64 = 12;

        let barrier = Arc::new(Barrier::new(2));
        let a_barrier = Arc::clone(&barrier);
        let thread_a = std::thread::spawn(move || {
            let mut sequences = Vec::new();
            for round in 0..ROUNDS {
                a_barrier.wait();
                sequences.push(
                    store_a
                        .record_heartbeat(test_ping(
                            "g",
                            &format!("a-{round}"),
                            &format!("pa-{round}"),
                            10 + round,
                        ))
                        .unwrap()
                        .sequence,
                );
                store_a.snapshot_now().unwrap();
            }
            sequences
        });
        let b_barrier = Arc::clone(&barrier);
        let thread_b = std::thread::spawn(move || {
            for round in 0..ROUNDS {
                b_barrier.wait();
                for burst in 0..BURST {
                    let idx = round * BURST + burst;
                    // Raw rows carry caller-owned sequences. Stride by 1_000
                    // so each stays strictly above anything the sequenced
                    // writer (disk-max + 1 per append) can reach in between —
                    // raw rows must land above every snapshot cutoff or
                    // replay skips them by design (see `append_ledger_row`).
                    let raw = SupervisorEventLedgerRow {
                        event_id: format!("raw:{idx}"),
                        sequence: 1_000_000 + idx * 1_000,
                        recorded_at_ms: 50 + idx,
                        event: SupervisorEvent::Heartbeat {
                            ping: test_ping(
                                "g",
                                &format!("raw-{idx}"),
                                &format!("pr-{idx}"),
                                50 + idx,
                            ),
                        },
                    };
                    store_b.append_ledger_row(&raw).unwrap();
                }
            }
        });

        let a_sequences = thread_a.join().unwrap();
        thread_b.join().unwrap();

        // Assigned sequences are strictly increasing and duplicate-free.
        assert!(
            a_sequences.windows(2).all(|pair| pair[0] < pair[1]),
            "assigned sequences not strictly increasing: {a_sequences:?}"
        );
        let unique: HashSet<u64> = a_sequences.iter().copied().collect();
        assert_eq!(
            unique.len(),
            a_sequences.len(),
            "duplicate sequences: {a_sequences:?}"
        );

        // No row lost: every child written by either thread — sequenced or
        // raw — must survive the racing compactions into the final state.
        let state = SupervisorStore::new(&dir.path).load_state().unwrap();
        for round in 0..ROUNDS {
            assert!(
                state
                    .children
                    .contains_key(&child_key("g", &format!("a-{round}"))),
                "lost sequenced child a-{round}"
            );
        }
        for idx in 0..ROUNDS * BURST {
            assert!(
                state
                    .children
                    .contains_key(&child_key("g", &format!("raw-{idx}"))),
                "lost RAW child raw-{idx} to a racing compaction"
            );
        }
    }

    #[test]
    fn same_length_ledger_with_different_tail_sequence_forces_reseed() {
        let dir = TestDir::new("aba");
        let store = SupervisorStore::new(&dir.path);
        store
            .record_heartbeat(test_ping("g", "c-1", "p-1", 1))
            .unwrap();
        store
            .record_heartbeat(test_ping("g", "c-2", "p-2", 2))
            .unwrap();

        // Out-of-band, rewrite the ledger to the SAME byte length but with a
        // different final sequence (2 -> 7): a foreign compact-then-append
        // cycle can land on an identical length, so length alone must never
        // validate the cursor (the ABA the fast path's content check kills).
        let body = fs::read_to_string(store.events_path()).unwrap();
        let forged = body.replace("\"sequence\":2", "\"sequence\":7");
        assert_ne!(body, forged, "fixture must actually change the tail");
        assert_eq!(body.len(), forged.len(), "fixture must keep the length");
        fs::write(store.events_path(), forged).unwrap();

        let row = store
            .record_heartbeat(test_ping("g", "c-3", "p-3", 3))
            .unwrap();
        assert_eq!(
            row.sequence, 8,
            "stale cursor must reseed from disk, not reuse cached+1"
        );
    }

    #[test]
    fn append_seals_a_complete_row_missing_its_trailing_newline() {
        let dir = TestDir::new("torn-tail");
        let store = SupervisorStore::new(&dir.path);
        store
            .record_heartbeat(test_ping("g", "c-1", "p-1", 1))
            .unwrap();

        // Simulate a crash that persisted a complete final row but lost the
        // trailing newline.
        let mut content = fs::read(store.events_path()).unwrap();
        assert_eq!(content.pop(), Some(b'\n'));
        fs::write(store.events_path(), &content).unwrap();

        // The next append must seal the torn tail with a newline first —
        // never concatenate JSON onto it, and never truncate it.
        let row = store
            .record_heartbeat(test_ping("g", "c-2", "p-2", 2))
            .unwrap();
        assert_eq!(row.sequence, 2);
        let rows = live_ledger_rows(&store);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter().map(|row| row.sequence).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn stable_event_id_stays_suppressed_across_snapshot_compaction() {
        let dir = TestDir::new("dedup-across-snapshot");
        let store = SupervisorStore::new(&dir.path);

        // Register a group whose event id is STABLE (`group_registered:<id>`
        // — the id constructor embeds no sequence or timestamp, so a
        // re-registration reuses it verbatim).
        let mut group = SupervisedGroupRecord::new("group-dup", 100);
        group.objective = Some("original objective".to_string());
        store.record_group_registered(group).unwrap();

        // Snapshot + compact: the applied-id set must ride along in the
        // snapshot — it is the durable dedup contract, not tail-epoch
        // bookkeeping.
        store.snapshot_now().unwrap();

        // Re-emit the SAME event id at a HIGHER sequence (fresher
        // updated_at, so a wrongly re-applied registration would visibly
        // clobber the state). The sequence cutoff cannot suppress it — only
        // the id set carried across the snapshot can.
        let mut clobber = SupervisedGroupRecord::new("group-dup", 999);
        clobber.objective = Some("clobbering duplicate".to_string());
        store.record_group_registered(clobber).unwrap();

        let state = SupervisorStore::new(&dir.path).load_state().unwrap();
        assert_eq!(
            state.groups["group-dup"].objective.as_deref(),
            Some("original objective"),
            "duplicate stable event id after a snapshot+compaction cycle must stay suppressed"
        );
        assert_eq!(state.groups["group-dup"].updated_at_ms, 100);
    }

    #[test]
    fn newer_snapshot_schema_version_is_refused() {
        let dir = TestDir::new("schema-guard");
        let store = SupervisorStore::new(&dir.path);
        store
            .record_heartbeat(test_ping("g", "c-1", "p-1", 1))
            .unwrap();
        store.snapshot_now().unwrap();

        // A snapshot written by a FUTURE binary: refuse to load rather than
        // silently misinterpret it.
        let mut snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(store.snapshot_path()).unwrap()).unwrap();
        snapshot["schema_version"] = serde_json::Value::from(SNAPSHOT_SCHEMA_VERSION + 1);
        fs::write(
            store.snapshot_path(),
            serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();

        let err = store.load_snapshot().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("schema_version"), "{err}");
        let err = store.load_state().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
