//! Background task lifecycle management for spawn_only tools.
//!
//! The `TaskSupervisor` is a status store that tracks background tasks from
//! spawn to completion. It does NOT enforce workspace contracts — that
//! responsibility belongs to `workspace_contract::enforce()`, which runs
//! inline in `execution.rs` BEFORE the supervisor status is updated.
//!
//! The supervisor only sees truth-checked states: `Completed` means the
//! workspace contract was satisfied, `Failed` means it was not.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use metrics::counter;
use octos_core::TaskId;
use serde::{Deserialize, Serialize};

const CURRENT_TASK_LEDGER_SCHEMA: u32 = 1;

/// Lifecycle status of a background task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Spawned,
    Running,
    Completed,
    Failed,
}

impl TaskStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Spawned | Self::Running)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// Structured terminal outcome for a child session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChildSessionTerminalState {
    Completed,
    RetryableFailure,
    TerminalFailure,
}

/// Join state for a child session contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChildSessionJoinState {
    Joined,
    Orphaned,
}

/// Fine-grained runtime phase of a background task.
///
/// `status` remains the coarse externally stable summary, while
/// `runtime_state` tracks where the task is inside the workspace/runtime
/// lifecycle. This lets the agent and UI distinguish "tool is still running"
/// from "tool finished but outputs are still being verified/delivered".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskRuntimeState {
    Spawned,
    ExecutingTool,
    ResolvingOutputs,
    VerifyingOutputs,
    DeliveringOutputs,
    CleaningUp,
    Completed,
    Failed,
}

/// A tracked background task spawned by a spawn_only tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub tool_name: String,
    pub tool_call_id: String,
    /// Parent session that owns this task.
    pub parent_session_key: Option<String>,
    /// Stable child session key derived from the parent session and task id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_session_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_terminal_state: Option<ChildSessionTerminalState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_join_state: Option<ChildSessionJoinState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_joined_at: Option<DateTime<Utc>>,
    /// Append-only ledger path used to persist this task's snapshots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_ledger_path: Option<String>,
    pub status: TaskStatus,
    pub runtime_state: TaskRuntimeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_detail: Option<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub output_files: Vec<String>,
    pub error: Option<String>,
    /// Session that owns this task (for per-session filtering).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
}

/// Callback invoked when a task's status changes.
type OnChangeCallback = Box<dyn Fn(&BackgroundTask) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedTaskRecord {
    #[serde(default = "default_task_ledger_schema")]
    schema_version: u32,
    task: BackgroundTask,
}

fn default_task_ledger_schema() -> u32 {
    CURRENT_TASK_LEDGER_SCHEMA
}

fn record_child_session_lifecycle(kind: &'static str, outcome: &'static str) {
    counter!(
        "octos_child_session_lifecycle_total",
        "kind" => kind.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

fn child_terminal_kind_label(state: &ChildSessionTerminalState) -> &'static str {
    match state {
        ChildSessionTerminalState::Completed => "completed",
        ChildSessionTerminalState::RetryableFailure => "retryable_failed",
        ChildSessionTerminalState::TerminalFailure => "terminal_failed",
    }
}

fn child_join_outcome_label(state: &ChildSessionJoinState) -> &'static str {
    match state {
        ChildSessionJoinState::Joined => "joined",
        ChildSessionJoinState::Orphaned => "orphaned",
    }
}

impl std::fmt::Debug for TaskSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskSupervisor")
            .field("tasks", &self.tasks)
            .field("on_change", &"<callback>")
            .field(
                "persistence_path",
                &self
                    .persistence_path
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|path| path.display().to_string()),
            )
            .finish()
    }
}

/// Supervisor that tracks background task lifecycle.
///
/// Thread-safe via interior `Mutex`. Cloning shares the same underlying state.
#[derive(Clone)]
pub struct TaskSupervisor {
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
    on_change: Arc<Mutex<Option<OnChangeCallback>>>,
    persistence_path: Arc<Mutex<Option<PathBuf>>>,
}

impl Default for TaskSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskSupervisor {
    /// Create an empty supervisor.
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            on_change: Arc::new(Mutex::new(None)),
            persistence_path: Arc::new(Mutex::new(None)),
        }
    }

    /// Enable append-only persistence for task snapshots and restore existing state.
    pub fn enable_persistence(&self, path: impl Into<PathBuf>) -> std::io::Result<usize> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let ledger_path = path.display().to_string();
        let restored = Self::load_persisted_tasks(&path)?;
        {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            for (task_id, task) in restored {
                match tasks.get(&task_id) {
                    Some(existing) if existing.updated_at >= task.updated_at => {}
                    _ => {
                        tasks.insert(task_id, task);
                    }
                }
            }
            for task in tasks.values_mut() {
                if task.task_ledger_path.as_deref() != Some(ledger_path.as_str()) {
                    task.task_ledger_path = Some(ledger_path.clone());
                }
            }
        }

        let mut guard = self
            .persistence_path
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(path);
        drop(guard);

        let snapshots: Vec<BackgroundTask> = {
            let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.values().cloned().collect()
        };
        for task in snapshots {
            self.persist_snapshot(&task);
        }

        Ok(self.tasks.lock().unwrap_or_else(|e| e.into_inner()).len())
    }

    /// Set a callback that fires whenever a task's status changes.
    pub fn set_on_change(&self, cb: impl Fn(&BackgroundTask) + Send + Sync + 'static) {
        let mut guard = self.on_change.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Box::new(cb));
    }

    /// Register a new background task. Returns the generated task ID.
    pub fn register(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
    ) -> String {
        self.register_with_lineage(tool_name, tool_call_id, session_key, None)
    }

    /// Register a new background task with optional ledger-path lineage.
    pub fn register_with_lineage(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
        task_ledger_path: Option<&str>,
    ) -> String {
        let id = TaskId::new().to_string();
        let derived_child_session_key = session_key.map(|parent| format!("{parent}#child-{id}"));
        let task = BackgroundTask {
            id: id.clone(),
            tool_name: tool_name.to_string(),
            tool_call_id: tool_call_id.to_string(),
            parent_session_key: session_key.map(|s| s.to_string()),
            child_session_key: derived_child_session_key,
            child_terminal_state: None,
            child_join_state: None,
            child_joined_at: None,
            task_ledger_path: task_ledger_path.map(|path| path.to_string()).or_else(|| {
                self.persistence_path
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|path| path.display().to_string())
            }),
            status: TaskStatus::Spawned,
            runtime_state: TaskRuntimeState::Spawned,
            runtime_detail: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
            output_files: Vec::new(),
            error: None,
            session_key: session_key.map(|s| s.to_string()),
        };
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.insert(id.clone(), task);
        drop(tasks);
        self.persist_snapshot_by_id(&id);
        record_child_session_lifecycle(
            "tracked",
            if session_key.is_some() {
                "registered"
            } else {
                "detached"
            },
        );
        id
    }

    /// Mark a task as running.
    pub fn mark_running(&self, task_id: &str) {
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = TaskStatus::Running;
                task.runtime_state = TaskRuntimeState::ExecutingTool;
                task.runtime_detail = None;
                task.updated_at = Utc::now();
                Some(task.clone())
            } else {
                None
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
        }
    }

    /// Update the fine-grained runtime state while keeping the coarse status.
    pub fn mark_runtime_state(
        &self,
        task_id: &str,
        runtime_state: TaskRuntimeState,
        runtime_detail: Option<String>,
    ) {
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.runtime_state = runtime_state;
                task.runtime_detail = runtime_detail;
                task.updated_at = Utc::now();
                Some(task.clone())
            } else {
                None
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
        }
    }

    /// Mark a task as completed with output files.
    pub fn mark_completed(&self, task_id: &str, output_files: Vec<String>) {
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = TaskStatus::Completed;
                task.runtime_state = TaskRuntimeState::Completed;
                task.updated_at = Utc::now();
                task.completed_at = Some(Utc::now());
                task.output_files = output_files;
                Some(task.clone())
            } else {
                None
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
        }
    }

    /// Mark a task as failed with an error message.
    pub fn mark_failed(&self, task_id: &str, error: String) {
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = TaskStatus::Failed;
                task.runtime_state = TaskRuntimeState::Failed;
                task.updated_at = Utc::now();
                task.completed_at = Some(Utc::now());
                task.error = Some(error);
                Some(task.clone())
            } else {
                None
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
        }
    }

    /// Record the child-session contract outcome for a task.
    pub fn mark_child_session_outcome(
        &self,
        task_id: &str,
        terminal_state: ChildSessionTerminalState,
        join_state: ChildSessionJoinState,
    ) {
        let kind_label = child_terminal_kind_label(&terminal_state);
        let outcome_label = child_join_outcome_label(&join_state);
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.child_terminal_state = Some(terminal_state);
                task.child_join_state = Some(join_state.clone());
                task.child_joined_at = match join_state {
                    ChildSessionJoinState::Joined => Some(Utc::now()),
                    ChildSessionJoinState::Orphaned => None,
                };
                task.updated_at = Utc::now();
                Some(task.clone())
            } else {
                None
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
            record_child_session_lifecycle(kind_label, outcome_label);
        }
    }

    fn persist_snapshot_by_id(&self, task_id: &str) {
        let snapshot = {
            let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.get(task_id).cloned()
        };
        if let Some(task) = snapshot {
            self.persist_snapshot(&task);
        }
    }

    fn persist_snapshot(&self, task: &BackgroundTask) {
        let Some(path) = self
            .persistence_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return;
        };

        let record = PersistedTaskRecord {
            schema_version: CURRENT_TASK_LEDGER_SCHEMA,
            task: task.clone(),
        };
        let Ok(json) = serde_json::to_string(&record) else {
            return;
        };

        if let Err(error) = Self::append_persisted_task(&path, &json) {
            tracing::warn!(
                task_id = %task.id,
                path = %path.display(),
                error = %error,
                "failed to persist background task snapshot"
            );
        }
    }

    /// Return a snapshot for a specific task id.
    pub fn get_task(&self, task_id: &str) -> Option<BackgroundTask> {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.get(task_id).cloned()
    }

    /// Return the persistence path for task snapshots, if enabled.
    pub fn persistence_path(&self) -> Option<PathBuf> {
        self.persistence_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn append_persisted_task(path: &PathBuf, json: &str) -> std::io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{json}")?;
        Ok(())
    }

    fn load_persisted_tasks(path: &PathBuf) -> std::io::Result<HashMap<String, BackgroundTask>> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HashMap::new());
            }
            Err(error) => return Err(error),
        };

        let mut restored = HashMap::new();
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<PersistedTaskRecord>(&line) else {
                continue;
            };
            if record.schema_version > CURRENT_TASK_LEDGER_SCHEMA {
                continue;
            }
            restored.insert(record.task.id.clone(), record.task);
        }
        Ok(restored)
    }

    /// Fire the on_change callback (if set) with a task snapshot.
    fn notify_change(&self, task: &BackgroundTask) {
        let guard = self.on_change.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref cb) = *guard {
            cb(task);
        }
    }

    /// Return all non-completed (active) tasks.
    pub fn get_active_tasks(&self) -> Vec<BackgroundTask> {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks
            .values()
            .filter(|t| t.status.is_active())
            .cloned()
            .collect()
    }

    /// Return all tracked tasks.
    pub fn get_all_tasks(&self) -> Vec<BackgroundTask> {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.values().cloned().collect()
    }

    /// Return all tasks belonging to a specific session.
    pub fn get_tasks_for_session(&self, session_key: &str) -> Vec<BackgroundTask> {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks
            .values()
            .filter(|t| t.session_key.as_deref() == Some(session_key))
            .cloned()
            .collect()
    }

    /// Number of active (non-completed, non-failed) tasks.
    pub fn task_count(&self) -> usize {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.values().filter(|t| t.status.is_active()).count()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_register_task_with_spawned_status() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("tts", "call-123", None);

        let tasks = supervisor.get_all_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, id);
        assert_eq!(tasks[0].tool_name, "tts");
        assert_eq!(tasks[0].tool_call_id, "call-123");
        assert_eq!(tasks[0].status, TaskStatus::Spawned);
        assert_eq!(tasks[0].runtime_state, TaskRuntimeState::Spawned);
        assert!(tasks[0].child_terminal_state.is_none());
        assert!(tasks[0].child_join_state.is_none());
        assert!(tasks[0].completed_at.is_none());
        assert!(tasks[0].updated_at >= tasks[0].started_at);
    }

    #[test]
    fn should_register_task_with_lineage_and_ledger_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger_path).unwrap();

        let id = supervisor.register_with_lineage(
            "podcast_generate",
            "call-42",
            Some("api:session"),
            Some(ledger_path.to_str().unwrap()),
        );

        let task = supervisor.get_task(&id).expect("task missing");
        let expected_child = format!("api:session#child-{id}");
        assert_eq!(task.parent_session_key.as_deref(), Some("api:session"));
        assert_eq!(
            task.child_session_key.as_deref(),
            Some(expected_child.as_str())
        );
        assert_eq!(
            task.task_ledger_path.as_deref(),
            Some(ledger_path.to_str().unwrap())
        );
    }

    #[test]
    fn should_transition_through_lifecycle_states() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("tts", "call-1", None);

        supervisor.mark_running(&id);
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.runtime_state, TaskRuntimeState::ExecutingTool);

        supervisor.mark_runtime_state(
            &id,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.runtime_state, TaskRuntimeState::DeliveringOutputs);
        assert_eq!(task.runtime_detail.as_deref(), Some("send_file"));

        supervisor.mark_completed(&id, vec!["output.mp3".to_string()]);
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.runtime_state, TaskRuntimeState::Completed);
        assert!(task.completed_at.is_some());
        assert_eq!(task.output_files, vec!["output.mp3"]);
    }

    #[test]
    fn should_persist_child_session_outcome_state() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("tts", "call-7", Some("api:session"));

        supervisor.mark_child_session_outcome(
            &id,
            ChildSessionTerminalState::RetryableFailure,
            ChildSessionJoinState::Joined,
        );

        let task = supervisor.get_task(&id).expect("task missing");
        assert_eq!(
            task.child_terminal_state,
            Some(ChildSessionTerminalState::RetryableFailure)
        );
        assert_eq!(task.child_join_state, Some(ChildSessionJoinState::Joined));
        assert!(task.child_joined_at.is_some());
    }

    #[test]
    fn should_track_failed_tasks_with_error() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("tts", "call-2", None);

        supervisor.mark_running(&id);
        supervisor.mark_failed(&id, "connection refused".to_string());

        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.runtime_state, TaskRuntimeState::Failed);
        assert_eq!(task.error.as_deref(), Some("connection refused"));
        assert!(task.completed_at.is_some());
    }

    #[test]
    fn should_count_only_active_tasks() {
        let supervisor = TaskSupervisor::new();
        let id1 = supervisor.register("tts", "call-1", None);
        let id2 = supervisor.register("tts", "call-2", None);
        let _id3 = supervisor.register("tts", "call-3", None);

        assert_eq!(supervisor.task_count(), 3);

        supervisor.mark_completed(&id1, vec![]);
        assert_eq!(supervisor.task_count(), 2);

        supervisor.mark_failed(&id2, "err".to_string());
        assert_eq!(supervisor.task_count(), 1);
    }

    #[test]
    fn should_return_only_active_tasks_in_get_active() {
        let supervisor = TaskSupervisor::new();
        let id1 = supervisor.register("tts", "call-1", None);
        let _id2 = supervisor.register("tts", "call-2", None);

        supervisor.mark_completed(&id1, vec![]);

        let active = supervisor.get_active_tasks();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].tool_call_id, "call-2");
    }

    #[test]
    fn should_be_empty_when_new() {
        let supervisor = TaskSupervisor::new();
        assert_eq!(supervisor.task_count(), 0);
        assert!(supervisor.get_all_tasks().is_empty());
        assert!(supervisor.get_active_tasks().is_empty());
    }

    #[test]
    fn should_ignore_unknown_task_ids() {
        let supervisor = TaskSupervisor::new();
        // These should not panic
        supervisor.mark_running("nonexistent");
        supervisor.mark_completed("nonexistent", vec![]);
        supervisor.mark_failed("nonexistent", "err".to_string());
        assert_eq!(supervisor.task_count(), 0);
    }

    #[test]
    fn should_restore_running_task_state_after_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger_path).unwrap();

        let task_id =
            supervisor.register_with_lineage("deep_search", "call-1", Some("api:session"), None);
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            TaskRuntimeState::ResolvingOutputs,
            Some("collecting evidence".to_string()),
        );

        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger_path).unwrap();

        let tasks = restored.get_all_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, task_id);
        assert_eq!(tasks[0].status, TaskStatus::Running);
        assert_eq!(tasks[0].runtime_state, TaskRuntimeState::ResolvingOutputs);
        assert_eq!(
            tasks[0].runtime_detail.as_deref(),
            Some("collecting evidence")
        );
        let expected_child = format!("api:session#child-{task_id}");
        assert_eq!(tasks[0].parent_session_key.as_deref(), Some("api:session"));
        assert_eq!(
            tasks[0].child_session_key.as_deref(),
            Some(expected_child.as_str())
        );
        assert_eq!(
            tasks[0].task_ledger_path.as_deref(),
            Some(ledger_path.to_str().unwrap())
        );
    }

    #[test]
    fn should_restore_completed_and_failed_truth_after_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger_path).unwrap();

        let completed =
            supervisor.register_with_lineage("fm_tts", "call-2", Some("api:session"), None);
        supervisor.mark_running(&completed);
        supervisor.mark_runtime_state(
            &completed,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );
        supervisor.mark_completed(&completed, vec!["/tmp/output.mp3".to_string()]);
        supervisor.mark_child_session_outcome(
            &completed,
            ChildSessionTerminalState::Completed,
            ChildSessionJoinState::Joined,
        );

        let failed = supervisor.register_with_lineage(
            "podcast_generate",
            "call-3",
            Some("api:session"),
            None,
        );
        supervisor.mark_running(&failed);
        supervisor.mark_failed(&failed, "No dialogue lines found in script".to_string());
        supervisor.mark_child_session_outcome(
            &failed,
            ChildSessionTerminalState::TerminalFailure,
            ChildSessionJoinState::Orphaned,
        );

        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger_path).unwrap();

        let tasks = restored.get_all_tasks();
        assert_eq!(tasks.len(), 2);

        let completed_task = tasks
            .iter()
            .find(|task| task.id == completed)
            .expect("completed task missing");
        assert_eq!(completed_task.status, TaskStatus::Completed);
        assert_eq!(completed_task.runtime_state, TaskRuntimeState::Completed);
        assert_eq!(completed_task.runtime_detail.as_deref(), Some("send_file"));
        assert_eq!(completed_task.output_files, vec!["/tmp/output.mp3"]);
        let expected_completed_child = format!("api:session#child-{completed}");
        assert_eq!(
            completed_task.parent_session_key.as_deref(),
            Some("api:session")
        );
        assert_eq!(
            completed_task.child_session_key.as_deref(),
            Some(expected_completed_child.as_str())
        );
        assert_eq!(
            completed_task.task_ledger_path.as_deref(),
            Some(ledger_path.to_str().unwrap())
        );
        assert_eq!(
            completed_task.child_terminal_state,
            Some(ChildSessionTerminalState::Completed)
        );
        assert_eq!(
            completed_task.child_join_state,
            Some(ChildSessionJoinState::Joined)
        );
        assert!(completed_task.child_joined_at.is_some());

        let failed_task = tasks
            .iter()
            .find(|task| task.id == failed)
            .expect("failed task missing");
        assert_eq!(failed_task.status, TaskStatus::Failed);
        assert_eq!(failed_task.runtime_state, TaskRuntimeState::Failed);
        assert_eq!(failed_task.runtime_detail, None);
        assert_eq!(
            failed_task.error.as_deref(),
            Some("No dialogue lines found in script")
        );
        assert_eq!(
            failed_task.parent_session_key.as_deref(),
            Some("api:session")
        );
        let expected_failed_child = format!("api:session#child-{failed}");
        assert_eq!(
            failed_task.child_session_key.as_deref(),
            Some(expected_failed_child.as_str())
        );
        assert_eq!(
            failed_task.task_ledger_path.as_deref(),
            Some(ledger_path.to_str().unwrap())
        );
        assert_eq!(
            failed_task.child_terminal_state,
            Some(ChildSessionTerminalState::TerminalFailure)
        );
        assert_eq!(
            failed_task.child_join_state,
            Some(ChildSessionJoinState::Orphaned)
        );
        assert!(failed_task.child_joined_at.is_none());
    }
}
