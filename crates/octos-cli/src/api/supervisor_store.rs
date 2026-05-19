#![allow(dead_code)]
//! Durable supervisor state store for supervised agent groups.
//!
//! The store is intentionally small: an append-only JSONL event ledger plus an
//! optional snapshot file. It is standalone so the runtime can wire it in later
//! without forcing API handlers to depend on supervisor internals.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const EVENTS_FILE_NAME: &str = "supervisor-events.jsonl";
const EVENTS_LOCK_FILE_NAME: &str = "supervisor-events.lock";
const SNAPSHOT_FILE_NAME: &str = "supervisor-snapshot.json";
const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const APPEND_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const APPEND_LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);
const AUTO_GROUP_TERMINAL_MESSAGE: &str = "all supervised children reached a terminal state";

pub type SupervisorMetadata = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
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

#[derive(Debug, Clone)]
pub struct SupervisorStore {
    root_dir: PathBuf,
    events_path: PathBuf,
    snapshot_path: PathBuf,
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
            snapshot_path: root_dir.join(SNAPSHOT_FILE_NAME),
            root_dir,
        }
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    pub fn events_path(&self) -> &Path {
        &self.events_path
    }

    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot_path
    }

    pub fn load_state(&self) -> io::Result<SupervisorState> {
        let snapshot = self.load_snapshot()?;
        let snapshot_last_sequence = snapshot.as_ref().map_or(0, |s| s.last_sequence);
        let mut state = snapshot.map_or_else(SupervisorState::default, |s| s.state);
        state.last_sequence = state.last_sequence.max(snapshot_last_sequence);

        for row in self.read_ledger_rows()? {
            if row.sequence > snapshot_last_sequence {
                state.apply_ledger_row(&row);
            }
        }
        Ok(state)
    }

    pub fn load_snapshot(&self) -> io::Result<Option<SupervisorSnapshot>> {
        if !self.snapshot_path.exists() {
            return Ok(None);
        }
        let body = fs::read_to_string(&self.snapshot_path)?;
        let snapshot = serde_json::from_str(&body).map_err(invalid_data)?;
        Ok(Some(snapshot))
    }

    pub fn write_snapshot(&self) -> io::Result<SupervisorSnapshot> {
        let state = self.load_state()?;
        let snapshot = SupervisorSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            written_at_ms: unix_time_millis(),
            last_sequence: state.last_sequence,
            state,
        };
        self.ensure_root_dir()?;
        let body = serde_json::to_string_pretty(&snapshot).map_err(invalid_data)?;
        let tmp_path = self.snapshot_path.with_extension("json.tmp");
        fs::write(&tmp_path, body)?;
        fs::rename(&tmp_path, &self.snapshot_path)?;
        Ok(snapshot)
    }

    pub fn append_event(
        &self,
        event_id: impl Into<String>,
        event: SupervisorEvent,
    ) -> io::Result<SupervisorEventLedgerRow> {
        let _lock = self.acquire_append_lock()?;
        let sequence = self.load_state()?.last_sequence.saturating_add(1);
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
        self.append_ledger_row(&row)?;
        Ok(row)
    }

    pub fn append_ledger_row(&self, row: &SupervisorEventLedgerRow) -> io::Result<()> {
        self.ensure_root_dir()?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_path)?;
        serde_json::to_writer(&mut file, row).map_err(invalid_data)?;
        file.write_all(b"\n")?;
        file.flush()
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

    fn read_ledger_rows(&self) -> io::Result<Vec<SupervisorEventLedgerRow>> {
        if !self.events_path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&self.events_path)?;
        let reader = BufReader::new(file);
        let mut rows = Vec::new();
        for (idx, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let row = serde_json::from_str(&line).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "failed to parse {} line {}: {err}",
                        self.events_path.display(),
                        idx + 1
                    ),
                )
            })?;
            rows.push(row);
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
            let _ = fs::remove_dir_all(&self.path);
        }
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
}
