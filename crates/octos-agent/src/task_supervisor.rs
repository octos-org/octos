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
use serde_json::Value;
use tokio::task::AbortHandle;

use crate::harness_events::{HarnessEvent, HarnessEventPayload};

const CURRENT_TASK_LEDGER_SCHEMA: u32 = 1;

/// Lifecycle status of a background task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Spawned,
    Running,
    Completed,
    Failed,
    /// Task was cancelled via `TaskSupervisor::cancel_task`.
    ///
    /// Terminal state, distinct from `Failed` (non-user-initiated) and
    /// `Completed` (workspace contract satisfied). Cancelled tasks may be
    /// re-launched via `relaunch_task`, preserving the original `task_id`
    /// as `parent_task_id` on the new task.
    Cancelled,
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
            Self::Cancelled => "cancelled",
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

/// Explicit follow-up policy for terminal child-session failures.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChildSessionFailureAction {
    Retry,
    Escalate,
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
    /// Task's `JoinHandle` was aborted via `cancel_task`.
    Cancelled,
}

/// Stable externally-facing lifecycle state for background tasks.
///
/// This is the coarse public contract that callers and UIs should consume.
/// It intentionally groups several internal runtime phases under `verifying`
/// so the runtime can evolve without leaking extra state-machine detail.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycleState {
    Queued,
    Running,
    Verifying,
    Ready,
    Failed,
    /// User-initiated cancellation via the PM supervisor.
    Cancelled,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_failure_action: Option<ChildSessionFailureAction>,
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
    /// Lineage marker used when a task was re-launched via
    /// [`TaskSupervisor::relaunch_task`]. Points at the original task id so
    /// downstream UIs can render the chain of re-launches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    /// Serialized task spec (tool input) captured at spawn time so
    /// `relaunch_task` can replay the same configuration with user-provided
    /// overrides. Stored as opaque JSON so the spec schema can evolve per
    /// tool without forcing a schema bump on the supervisor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_snapshot: Option<Value>,
    /// Reason recorded when a task transitions into the `Cancelled` terminal
    /// state. Typically the caller of `cancel_task` supplies this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancellation_reason: Option<String>,
}

impl BackgroundTask {
    pub fn lifecycle_state(&self) -> TaskLifecycleState {
        match self.status {
            TaskStatus::Spawned => TaskLifecycleState::Queued,
            TaskStatus::Completed => TaskLifecycleState::Ready,
            TaskStatus::Failed => TaskLifecycleState::Failed,
            TaskStatus::Cancelled => TaskLifecycleState::Cancelled,
            TaskStatus::Running => match self.runtime_state {
                TaskRuntimeState::Spawned | TaskRuntimeState::ExecutingTool => {
                    TaskLifecycleState::Running
                }
                TaskRuntimeState::ResolvingOutputs
                | TaskRuntimeState::VerifyingOutputs
                | TaskRuntimeState::DeliveringOutputs
                | TaskRuntimeState::CleaningUp
                | TaskRuntimeState::Completed => TaskLifecycleState::Verifying,
                TaskRuntimeState::Failed => TaskLifecycleState::Failed,
                TaskRuntimeState::Cancelled => TaskLifecycleState::Cancelled,
            },
        }
    }
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

fn record_child_session_orphan(reason: &'static str) {
    counter!(
        "octos_child_session_orphan_total",
        "reason" => reason.to_string()
    )
    .increment(1);
}

fn record_task_cancellation(origin: &'static str) {
    counter!(
        "octos_task_cancellation_total",
        "origin" => origin.to_string()
    )
    .increment(1);
}

fn record_send_to_agent(outcome: &'static str) {
    counter!(
        "octos_send_to_agent_total",
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

fn record_task_relaunch(outcome: &'static str) {
    counter!(
        "octos_task_relaunch_total",
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

/// Errors that can be produced by [`TaskSupervisor::cancel_task`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelError {
    /// No task exists with the supplied id in the supervisor's registry.
    UnknownTask,
    /// The task has already reached a terminal state; cancellation is
    /// idempotent but callers can distinguish "did something" from
    /// "already done".
    AlreadyTerminal(TaskStatus),
}

impl std::fmt::Display for CancelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTask => write!(f, "unknown task id"),
            Self::AlreadyTerminal(status) => {
                write!(f, "task is already terminal (status={})", status.as_str())
            }
        }
    }
}

impl std::error::Error for CancelError {}

/// Errors that can be produced by [`TaskSupervisor::send_to_agent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendToAgentError {
    UnknownTask,
    /// The task exists but it was registered without an inbox sender —
    /// typically because it runs on an MCP-backed backend that does not
    /// yet plumb steering messages.
    NoInbox,
    InboxClosed,
    Terminal(TaskStatus),
}

impl std::fmt::Display for SendToAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTask => write!(f, "unknown task id"),
            Self::NoInbox => write!(f, "task registered without a steering inbox"),
            Self::InboxClosed => write!(f, "task inbox receiver has been dropped"),
            Self::Terminal(status) => {
                write!(f, "task is already terminal (status={})", status.as_str())
            }
        }
    }
}

impl std::error::Error for SendToAgentError {}

/// Errors that can be produced by [`TaskSupervisor::relaunch_task`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelaunchError {
    UnknownTask,
    /// The original task was registered without a spec snapshot, so we
    /// cannot reconstruct its tool input. Typical causes: the task came
    /// from a pre-M7.9 spawn that never called `register_abort`, or a
    /// non-spawn_only background wrapper.
    NoSpecSnapshot,
    /// The seed override patch was not a JSON object and therefore cannot
    /// merge onto the stored spec snapshot.
    InvalidOverrides,
}

impl std::fmt::Display for RelaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTask => write!(f, "unknown task id"),
            Self::NoSpecSnapshot => {
                write!(f, "task has no stored spec snapshot for re-launch")
            }
            Self::InvalidOverrides => {
                write!(f, "relaunch overrides must be a JSON object")
            }
        }
    }
}

impl std::error::Error for RelaunchError {}

/// Prepared re-launch spec ready to be handed back to the spawn-only tool.
/// Includes the merged spec, the new task id, and the lineage link.
#[derive(Debug, Clone)]
pub struct RelaunchPlan {
    pub new_task_id: String,
    pub parent_task_id: String,
    pub tool_name: String,
    pub session_key: Option<String>,
    pub merged_spec: Value,
}

fn record_workflow_phase_transition(workflow_kind: &str, from_phase: &str, to_phase: &str) {
    counter!(
        "octos_workflow_phase_transition_total",
        "workflow_kind" => workflow_kind.to_string(),
        "from_phase" => from_phase.to_string(),
        "to_phase" => to_phase.to_string()
    )
    .increment(1);
}

fn workflow_labels(detail: Option<&str>) -> (Option<String>, Option<String>) {
    let parsed = detail
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    let workflow_kind = parsed
        .get("workflow_kind")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let current_phase = parsed
        .get("current_phase")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    (workflow_kind, current_phase)
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

/// Shallow-merge a JSON object `overrides` onto `base`.
///
/// Kept intentionally simple: top-level object keys from `overrides` win,
/// but we merge nested objects recursively so callers can patch a single
/// field without clobbering siblings. Non-object `overrides` are rejected
/// so a typo cannot silently replace the entire spec.
fn merge_seed_overrides(base: Value, overrides: Value) -> Result<Value, RelaunchError> {
    if overrides.is_null() {
        return Ok(base);
    }
    let overrides = match overrides {
        Value::Object(map) => map,
        _ => return Err(RelaunchError::InvalidOverrides),
    };

    let mut base_map = match base {
        Value::Object(map) => map,
        other => {
            // If the base was not an object (rare — typically a plugin
            // returned a scalar input), we replace wholesale rather than
            // silently discard the overrides.
            return Ok(Value::Object(
                overrides
                    .into_iter()
                    .chain(std::iter::once(("__previous_spec".to_string(), other)))
                    .collect(),
            ));
        }
    };

    for (key, value) in overrides {
        match (base_map.remove(&key), value) {
            (Some(Value::Object(nested_base)), Value::Object(nested_override)) => {
                let merged = merge_seed_overrides(
                    Value::Object(nested_base),
                    Value::Object(nested_override),
                )?;
                base_map.insert(key, merged);
            }
            (_, other) => {
                base_map.insert(key, other);
            }
        }
    }

    Ok(Value::Object(base_map))
}

fn child_failure_action_for_terminal_state(
    state: &ChildSessionTerminalState,
) -> Option<ChildSessionFailureAction> {
    match state {
        ChildSessionTerminalState::Completed => None,
        ChildSessionTerminalState::RetryableFailure => Some(ChildSessionFailureAction::Retry),
        ChildSessionTerminalState::TerminalFailure => Some(ChildSessionFailureAction::Escalate),
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

/// Inbox message delivered to a running sub-agent via
/// [`TaskSupervisor::send_to_agent`]. Drained at the start of each agent
/// loop turn and prepended as a user-role message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    /// Caller-supplied origin label (operator id, Matrix user, etc.). Used
    /// for attribution in logs and system-message formatting.
    pub sender: String,
    pub body: String,
    pub received_at: DateTime<Utc>,
}

impl InboxMessage {
    pub fn new(sender: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            sender: sender.into(),
            body: body.into(),
            received_at: Utc::now(),
        }
    }
}

/// Handle attached to a running sub-agent's inbox. The supervisor stores a
/// clone of the sender so out-of-band callers (REST handler, Matrix reply
/// consumer, CLI tool) can route messages into the agent loop without
/// needing direct access to the spawn future.
#[derive(Clone)]
pub struct SupervisorInbox {
    sender: tokio::sync::mpsc::UnboundedSender<InboxMessage>,
}

impl SupervisorInbox {
    pub fn new(sender: tokio::sync::mpsc::UnboundedSender<InboxMessage>) -> Self {
        Self { sender }
    }

    /// Deliver a message to the bound agent. Returns `false` when the
    /// receiver has been dropped (agent already completed or aborted).
    pub fn send(&self, msg: InboxMessage) -> bool {
        self.sender.send(msg).is_ok()
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

/// Shared handle stored inside [`TaskSupervisor`] alongside the task record
/// so `cancel_task` and `send_to_agent` can reach into a running background
/// sub-agent. Holds `AbortHandle` for termination + [`SupervisorInbox`] for
/// steering. Both fields are populated when [`TaskSupervisor::register_abort`]
/// is called from the spawn-only tool wrapper.
struct TaskHandles {
    abort: Option<AbortHandle>,
    inbox: Option<SupervisorInbox>,
    spec_snapshot: Option<Value>,
}

/// Supervisor that tracks background task lifecycle.
///
/// Thread-safe via interior `Mutex`. Cloning shares the same underlying state.
#[derive(Clone)]
pub struct TaskSupervisor {
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
    on_change: Arc<Mutex<Option<OnChangeCallback>>>,
    persistence_path: Arc<Mutex<Option<PathBuf>>>,
    /// Per-task runtime handles for cancel / steer. Separate from `tasks`
    /// because `AbortHandle` is neither `Serialize` nor persistable — the
    /// lifecycle state itself lives in `tasks`, handles are only valid for
    /// the current process lifetime.
    handles: Arc<Mutex<HashMap<String, TaskHandles>>>,
    /// Path to the newline-JSON harness event sink used to emit
    /// `task.lifecycle.cancelled` events. Attached via
    /// `attach_harness_event_sink` — without it, `cancel_task` still
    /// performs the abort + state transition but skips the event write.
    harness_event_sink_path: Arc<Mutex<Option<String>>>,
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
            handles: Arc::new(Mutex::new(HashMap::new())),
            harness_event_sink_path: Arc::new(Mutex::new(None)),
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
        self.register_with_lineage_and_parent(
            tool_name,
            tool_call_id,
            session_key,
            task_ledger_path,
            None,
        )
    }

    /// Register a new background task with optional parent-task lineage.
    ///
    /// `parent_task_id` is used by [`TaskSupervisor::relaunch_task`] to
    /// preserve the re-launch chain for traceability. Callers creating a
    /// fresh top-level task should pass `None`.
    pub fn register_with_lineage_and_parent(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
        task_ledger_path: Option<&str>,
        parent_task_id: Option<&str>,
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
            child_failure_action: None,
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
            parent_task_id: parent_task_id.map(|id| id.to_string()),
            spec_snapshot: None,
            cancellation_reason: None,
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
        let (snapshot, previous_detail) = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                let previous_detail = task.runtime_detail.clone();
                task.runtime_state = runtime_state;
                task.runtime_detail = runtime_detail;
                task.updated_at = Utc::now();
                (Some(task.clone()), previous_detail)
            } else {
                (None, None)
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
            let (previous_kind, previous_phase) = workflow_labels(previous_detail.as_deref());
            let (current_kind, current_phase) = workflow_labels(task.runtime_detail.as_deref());
            if let (Some(workflow_kind), Some(to_phase)) =
                (current_kind.as_deref(), current_phase.as_deref())
            {
                let from_phase = if previous_kind.as_deref() == Some(workflow_kind) {
                    previous_phase.as_deref().unwrap_or("untracked")
                } else {
                    "untracked"
                };
                if from_phase != to_phase {
                    record_workflow_phase_transition(workflow_kind, from_phase, to_phase);
                }
            }
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
        let failure_action = child_failure_action_for_terminal_state(&terminal_state);
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
                task.child_failure_action = failure_action;
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
            if matches!(join_state, ChildSessionJoinState::Orphaned) {
                record_child_session_orphan("terminal_event_not_joined");
            }
        }
    }

    /// Apply a structured harness event to a tracked task.
    pub fn apply_harness_event(
        &self,
        task_id: &str,
        event: &HarnessEvent,
    ) -> Result<(), &'static str> {
        let snapshot = self.get_task(task_id).ok_or("unknown task")?;
        let (workflow_kind, current_phase) = workflow_labels(snapshot.runtime_detail.as_deref());
        let runtime_detail =
            event.runtime_detail_value(workflow_kind.as_deref(), current_phase.as_deref());

        match &event.payload {
            HarnessEventPayload::Progress { .. }
            | HarnessEventPayload::Phase { .. }
            | HarnessEventPayload::Retry { .. } => {
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::Artifact { .. } => {
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::DeliveringOutputs,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::ValidatorResult { data } => {
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::VerifyingOutputs,
                    Some(runtime_detail.to_string()),
                );
                if !data.passed {
                    let message = data.message.clone().unwrap_or_else(|| {
                        "validator rejected structured harness event".to_string()
                    });
                    self.mark_failed(task_id, message);
                }
            }
            HarnessEventPayload::Failure { data } => {
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::Failed,
                    Some(runtime_detail.to_string()),
                );
                self.mark_failed(task_id, data.message.clone());
            }
            HarnessEventPayload::McpServerCall { .. } => {
                // MCP-server dispatch events are audit records — they describe
                // a call that already mapped onto the supervisor via
                // run-to-completion. Nothing to reapply to lifecycle state.
            }
            HarnessEventPayload::SubAgentDispatch { .. } => {
                // Dispatch events are observational — they record the fact
                // that a task was shipped off to an MCP-backed sub-agent
                // without mutating the task's terminal state. The outer
                // spawn lifecycle still decides when the task completes or
                // fails; we just attach the structured detail so operators
                // can see which backend is servicing the task.
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::SwarmDispatch { .. } => {
                // Swarm dispatch events are observational from the
                // supervisor's perspective — the `octos-swarm` primitive
                // owns its own redb-backed session state and drives the
                // retry loop. We just surface the aggregate detail so
                // operators can see fan-out progress.
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::SwarmReviewDecision { .. } => {
                // Review decisions are supervisor-authored audit records.
                // They do not move the task lifecycle — the originating
                // dispatch already reached a terminal state when the
                // review panel was shown. Surface the detail so operators
                // can see accept/reject transitions on the timeline.
                self.mark_runtime_state(
                    task_id,
                    snapshot.runtime_state,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::CostAttribution { .. } => {
                // Cost attributions are purely observational — they are
                // committed after a sub-agent dispatch succeeds and do
                // not move the task's lifecycle. Attach the structured
                // detail so operators see the spend breakdown on the
                // same task row as the dispatch.
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::RoutingDecision { .. } => {
                // Routing decisions are observational — they do not change the
                // task's lifecycle state. We still attach the detail so the
                // operator dashboard can surface the tier/reasons for this
                // turn without inventing a dedicated sidecar channel.
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::CredentialRotation { .. } => {
                // Credential rotations are observability-only — they do not
                // change the task lifecycle. We still update runtime_detail
                // so operators can see which key is now active.
                self.mark_runtime_state(
                    task_id,
                    snapshot.runtime_state,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::Error { data } => {
                // Structured error events are diagnostic — record them in the
                // runtime detail but only transition to Failed when the
                // recovery hint marks the variant as non-retryable.
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
                if matches!(data.recovery.as_str(), "fail_fast" | "bug") {
                    self.mark_failed(task_id, data.message.clone());
                }
            }
            HarnessEventPayload::TaskLifecycleCancelled { data } => {
                // Cancellation events are emitted by the supervisor itself
                // when `cancel_task` runs. Replaying one from an external
                // sink should be idempotent: we only transition if the task
                // is not already terminal to avoid racing with a concurrent
                // cancel_task call that already wrote the same record.
                if snapshot.status.is_active() {
                    let reason = if data.reason.is_empty() {
                        "cancelled by operator".to_string()
                    } else {
                        data.reason.clone()
                    };
                    let _ = self.cancel_task(task_id, Some(reason));
                }
            }
        }

        Ok(())
    }

    /// Attach the abort handle + inbox for a running task.
    ///
    /// Called immediately after `tokio::spawn(...)` in the spawn-only tool
    /// wrapper — the wrapper captures `AbortHandle::from(&handle)` and the
    /// matching [`SupervisorInbox`] sender so operators can later steer or
    /// kill the task via [`Self::cancel_task`] / [`Self::send_to_agent`].
    pub fn register_abort(
        &self,
        task_id: &str,
        abort: AbortHandle,
        inbox: Option<SupervisorInbox>,
        spec_snapshot: Option<Value>,
    ) {
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        handles.insert(
            task_id.to_string(),
            TaskHandles {
                abort: Some(abort),
                inbox,
                spec_snapshot: spec_snapshot.clone(),
            },
        );
        drop(handles);
        if let Some(snap) = spec_snapshot {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.spec_snapshot = Some(snap);
                task.updated_at = Utc::now();
            }
        }
    }

    /// Remove the runtime handle for a task once it reaches a terminal
    /// state. Safe to call multiple times — the record is a no-op after
    /// the first removal.
    pub fn unregister_abort(&self, task_id: &str) {
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        handles.remove(task_id);
    }

    /// Cancel a running background task. Aborts the underlying `JoinHandle`
    /// and transitions the supervisor record to [`TaskStatus::Cancelled`].
    ///
    /// Returns an error if the task is unknown or already terminal. The
    /// abort is best-effort: a task that has already yielded is not
    /// preempted, but we still record the terminal transition so operators
    /// get a consistent audit record.
    pub fn cancel_task(&self, task_id: &str, reason: Option<String>) -> Result<(), CancelError> {
        self.cancel_task_with_origin(task_id, reason, "operator", None)
    }

    /// Cancel a running task with a caller-specified origin label and an
    /// optional relaunch pointer. Used by the relaunch flow to emit the
    /// `task.lifecycle.cancelled` event with `relaunched_as` populated.
    pub fn cancel_task_with_origin(
        &self,
        task_id: &str,
        reason: Option<String>,
        origin: &'static str,
        relaunched_as: Option<String>,
    ) -> Result<(), CancelError> {
        let existing = self.get_task(task_id).ok_or(CancelError::UnknownTask)?;
        if matches!(
            existing.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            return Err(CancelError::AlreadyTerminal(existing.status));
        }

        let abort_handle = {
            let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
            handles.get_mut(task_id).and_then(|h| h.abort.take())
        };
        if let Some(handle) = abort_handle {
            handle.abort();
        }

        let effective_reason = reason.unwrap_or_else(|| "cancelled by operator".to_string());
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = TaskStatus::Cancelled;
                task.runtime_state = TaskRuntimeState::Cancelled;
                task.cancellation_reason = Some(effective_reason.clone());
                task.updated_at = Utc::now();
                task.completed_at = Some(Utc::now());
                Some(task.clone())
            } else {
                None
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
            record_task_cancellation(origin);
            self.emit_cancelled_event(task, &effective_reason, origin, relaunched_as.as_deref());
        }
        Ok(())
    }

    fn emit_cancelled_event(
        &self,
        task: &BackgroundTask,
        reason: &str,
        origin: &'static str,
        relaunched_as: Option<&str>,
    ) {
        let session_id = task
            .parent_session_key
            .clone()
            .or_else(|| task.session_key.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let event = HarnessEvent::task_lifecycle_cancelled(
            session_id,
            task.id.clone(),
            reason,
            origin,
            relaunched_as.map(str::to_string),
        );
        let sink = {
            let guard = self
                .harness_event_sink_path
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        if let Some(sink) = sink {
            if let Err(error) = crate::harness_events::write_event_to_sink(&sink, &event) {
                tracing::warn!(
                    task_id = %task.id,
                    sink = %sink,
                    error = %error,
                    "failed to persist task.lifecycle.cancelled event"
                );
            }
        }
    }

    /// Attach a harness event sink path used by `cancel_task` to emit
    /// `task.lifecycle.cancelled` events. Must be the same newline-JSON
    /// sink the rest of the harness events stream writes to.
    pub fn attach_harness_event_sink(&self, path: impl Into<String>) {
        let mut guard = self
            .harness_event_sink_path
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(path.into());
    }

    pub fn harness_event_sink_path(&self) -> Option<String> {
        let guard = self
            .harness_event_sink_path
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.clone()
    }

    /// Deliver an inbox message to a running sub-agent.
    ///
    /// Returns `SendToAgentError::UnknownTask` if the supervisor has no
    /// record, `NoInbox` if the task was registered without an inbox
    /// sender (e.g. MCP-backed sub-agent where steering is unsupported),
    /// or `Terminal` if the task already finished.
    pub fn send_to_agent(
        &self,
        task_id: &str,
        message: InboxMessage,
    ) -> Result<(), SendToAgentError> {
        let existing = self
            .get_task(task_id)
            .ok_or(SendToAgentError::UnknownTask)?;
        if !existing.status.is_active() {
            return Err(SendToAgentError::Terminal(existing.status));
        }

        let handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        let Some(handle) = handles.get(task_id) else {
            return Err(SendToAgentError::NoInbox);
        };
        let Some(inbox) = handle.inbox.clone() else {
            return Err(SendToAgentError::NoInbox);
        };
        drop(handles);

        if !inbox.send(message) {
            return Err(SendToAgentError::InboxClosed);
        }
        record_send_to_agent("accepted");
        Ok(())
    }

    /// Return the spec snapshot recorded at `register_abort` time, used by
    /// `relaunch_task` to replay the task with caller-supplied overrides.
    pub fn spec_snapshot(&self, task_id: &str) -> Option<Value> {
        let handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        handles
            .get(task_id)
            .and_then(|h| h.spec_snapshot.clone())
            .or_else(|| {
                // Fall back to the stored task record — still present even
                // after the runtime handle was unregistered for a completed
                // task (needed when re-launching from a cancelled peer).
                let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
                tasks.get(task_id).and_then(|t| t.spec_snapshot.clone())
            })
    }

    /// Prepare a re-launch of a tracked task with caller-supplied
    /// `seed_overrides` patched onto the stored spec snapshot.
    ///
    /// This is the control-plane half of the relaunch flow — it registers
    /// a fresh task id linked to the original via `parent_task_id`,
    /// returns the merged spec + new id, and the caller (typically the
    /// `SpawnTool` wrapper or a REST handler) is responsible for actually
    /// invoking the tool with the merged spec. We split the control plane
    /// from the data plane so the supervisor module does not need to
    /// depend on `SpawnTool` internals.
    pub fn relaunch_task(
        &self,
        task_id: &str,
        seed_overrides: Value,
    ) -> Result<RelaunchPlan, RelaunchError> {
        let original = self.get_task(task_id).ok_or(RelaunchError::UnknownTask)?;
        let spec = self
            .spec_snapshot(task_id)
            .ok_or(RelaunchError::NoSpecSnapshot)?;

        let merged_spec = merge_seed_overrides(spec, seed_overrides)?;

        let new_task_id = self.register_with_lineage_and_parent(
            &original.tool_name,
            &original.tool_call_id,
            original.session_key.as_deref(),
            original.task_ledger_path.as_deref(),
            Some(task_id),
        );

        // Stamp the merged spec onto the fresh task so subsequent
        // re-launches can chain without re-reading the original.
        {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(&new_task_id) {
                task.spec_snapshot = Some(merged_spec.clone());
                task.updated_at = Utc::now();
            }
        }
        self.persist_snapshot_by_id(&new_task_id);
        record_task_relaunch("accepted");

        // If the original is still active, cancel it with a relaunch
        // origin so the emitted `task.lifecycle.cancelled` event carries
        // the `relaunched_as` pointer. Ignore AlreadyTerminal — the caller
        // may have already cancelled/waited.
        let _ = self.cancel_task_with_origin(
            task_id,
            Some(format!("relaunched as {new_task_id}")),
            "relaunch",
            Some(new_task_id.clone()),
        );

        Ok(RelaunchPlan {
            new_task_id,
            parent_task_id: task_id.to_string(),
            tool_name: original.tool_name,
            session_key: original.session_key,
            merged_spec,
        })
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
        assert!(tasks[0].child_failure_action.is_none());
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
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Queued);

        supervisor.mark_running(&id);
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.runtime_state, TaskRuntimeState::ExecutingTool);
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Running);

        supervisor.mark_runtime_state(
            &id,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.runtime_state, TaskRuntimeState::DeliveringOutputs);
        assert_eq!(task.runtime_detail.as_deref(), Some("send_file"));
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Verifying);

        supervisor.mark_completed(&id, vec!["output.mp3".to_string()]);
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.runtime_state, TaskRuntimeState::Completed);
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Ready);
        assert!(task.completed_at.is_some());
        assert_eq!(task.output_files, vec!["output.mp3"]);
    }

    #[test]
    fn should_apply_harness_progress_event_and_notify() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("deep_search", "call-9", Some("api:session"));
        supervisor.mark_running(&id);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        supervisor.set_on_change(move |task| {
            let _ = tx.send(task.clone());
        });

        let event = crate::harness_events::HarnessEvent::progress(
            "api:session",
            id.clone(),
            Some("deep_research"),
            "fetching_sources",
            Some("Fetching source 3/12"),
            Some(0.42),
        );

        supervisor.apply_harness_event(&id, &event).unwrap();

        let task = supervisor.get_task(&id).expect("task missing");
        let detail: serde_json::Value =
            serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["workflow_kind"], "deep_research");
        assert_eq!(detail["current_phase"], "fetching_sources");
        assert_eq!(detail["progress_message"], "Fetching source 3/12");
        let progress = detail["progress"].as_f64().unwrap();
        assert!((progress - 0.42).abs() < 0.0001);

        let notified = rx.try_recv().expect("callback should fire");
        let notified_detail: serde_json::Value =
            serde_json::from_str(notified.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(notified_detail["current_phase"], "fetching_sources");
        assert_eq!(notified.lifecycle_state(), TaskLifecycleState::Running);
    }

    #[test]
    fn should_persist_harness_progress_event_for_replay() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger_path).unwrap();
        let id =
            supervisor.register_with_lineage("deep_search", "call-9", Some("api:session"), None);
        supervisor.mark_running(&id);

        let event = crate::harness_events::HarnessEvent::progress(
            "api:session",
            id.clone(),
            Some("deep_research"),
            "fetch",
            Some("Fetching 4 pages"),
            Some(0.4),
        );
        supervisor.apply_harness_event(&id, &event).unwrap();

        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger_path).unwrap();
        let task = restored.get_task(&id).expect("restored task missing");
        let detail: serde_json::Value =
            serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(
            detail["schema"],
            crate::harness_events::HARNESS_EVENT_SCHEMA_V1
        );
        assert_eq!(detail["session_id"], "api:session");
        assert_eq!(detail["task_id"], id);
        assert_eq!(detail["workflow_kind"], "deep_research");
        assert_eq!(detail["current_phase"], "fetch");
        assert_eq!(detail["progress_message"], "Fetching 4 pages");
        assert_eq!(task.status, TaskStatus::Running);
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
        assert_eq!(
            task.child_failure_action,
            Some(ChildSessionFailureAction::Retry)
        );
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
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Failed);
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
        assert_eq!(completed_task.child_failure_action, None);
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
        assert_eq!(
            failed_task.child_failure_action,
            Some(ChildSessionFailureAction::Escalate)
        );
        assert!(failed_task.child_joined_at.is_none());
    }

    // --- M7.9 PM supervisor primitives ---

    #[test]
    fn should_merge_seed_overrides_recursively() {
        let base = serde_json::json!({
            "task": "summarize",
            "files": ["a.txt"],
            "config": {
                "model": "gpt-4",
                "timeout": 30
            }
        });
        let overrides = serde_json::json!({
            "task": "rewrite",
            "config": {
                "timeout": 60
            }
        });
        let merged = merge_seed_overrides(base, overrides).unwrap();
        assert_eq!(merged["task"], "rewrite");
        assert_eq!(merged["files"], serde_json::json!(["a.txt"]));
        assert_eq!(merged["config"]["model"], "gpt-4");
        assert_eq!(merged["config"]["timeout"], 60);
    }

    #[test]
    fn should_reject_non_object_seed_overrides() {
        let base = serde_json::json!({"task": "x"});
        let err = merge_seed_overrides(base, serde_json::json!("not-an-object")).unwrap_err();
        assert_eq!(err, RelaunchError::InvalidOverrides);
    }

    #[test]
    fn should_cancel_running_task_and_record_terminal_state() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("spawn", "call-cancel", Some("api:session"));
        supervisor.mark_running(&id);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        let abort = handle.abort_handle();
        supervisor.register_abort(&id, abort, None, None);

        supervisor
            .cancel_task(&id, Some("operator kill".into()))
            .expect("cancel should succeed");

        let task = supervisor.get_task(&id).expect("task record missing");
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Cancelled);
        assert_eq!(task.cancellation_reason.as_deref(), Some("operator kill"));
        assert!(task.completed_at.is_some());

        // Second cancel should surface AlreadyTerminal.
        let err = supervisor
            .cancel_task(&id, None)
            .expect_err("double cancel should fail");
        assert_eq!(err, CancelError::AlreadyTerminal(TaskStatus::Cancelled));

        rt.shutdown_background();
    }

    #[test]
    fn should_return_unknown_task_error_for_bogus_cancel() {
        let supervisor = TaskSupervisor::new();
        let err = supervisor
            .cancel_task("does-not-exist", None)
            .expect_err("expected failure");
        assert_eq!(err, CancelError::UnknownTask);
    }

    #[test]
    fn should_deliver_send_to_agent_into_inbox() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("spawn", "call-inbox", Some("api:session"));
        supervisor.mark_running(&id);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let inbox = SupervisorInbox::new(tx);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        supervisor.register_abort(&id, handle.abort_handle(), Some(inbox), None);

        supervisor
            .send_to_agent(&id, InboxMessage::new("operator", "please try again"))
            .expect("send should succeed");

        let msg = rx.try_recv().expect("inbox should have received message");
        assert_eq!(msg.sender, "operator");
        assert_eq!(msg.body, "please try again");
        rt.shutdown_background();
    }

    #[test]
    fn should_reject_send_to_agent_without_inbox() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("spawn", "call-no-inbox", Some("api:session"));
        supervisor.mark_running(&id);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        supervisor.register_abort(&id, handle.abort_handle(), None, None);

        let err = supervisor
            .send_to_agent(&id, InboxMessage::new("operator", "hi"))
            .expect_err("should not deliver without inbox");
        assert_eq!(err, SendToAgentError::NoInbox);
        rt.shutdown_background();
    }

    #[test]
    fn should_reject_send_to_agent_for_terminal_task() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("spawn", "call-done", Some("api:session"));
        supervisor.mark_running(&id);
        supervisor.mark_completed(&id, vec![]);

        let err = supervisor
            .send_to_agent(&id, InboxMessage::new("operator", "late"))
            .expect_err("terminal task should reject steering");
        assert_eq!(err, SendToAgentError::Terminal(TaskStatus::Completed));
    }

    #[test]
    fn should_relaunch_with_overrides_and_preserve_lineage() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("spawn", "call-r", Some("api:session"));
        supervisor.mark_running(&id);
        let spec = serde_json::json!({
            "task": "original",
            "config": {"model": "m1"}
        });
        supervisor.register_abort(
            &id,
            {
                let rt = tokio::runtime::Runtime::new().unwrap();
                let h = rt.spawn(async {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                });
                let ab = h.abort_handle();
                // shutdown rt at end — the abort handle outlives it but the
                // underlying future is dropped. This is fine for a snapshot
                // test.
                rt.shutdown_background();
                ab
            },
            None,
            Some(spec),
        );

        let plan = supervisor
            .relaunch_task(
                &id,
                serde_json::json!({"config": {"model": "m2"}, "extra": true}),
            )
            .expect("relaunch should succeed");

        assert_ne!(plan.new_task_id, id);
        assert_eq!(plan.parent_task_id, id);
        assert_eq!(plan.tool_name, "spawn");
        assert_eq!(plan.merged_spec["task"], "original");
        assert_eq!(plan.merged_spec["config"]["model"], "m2");
        assert_eq!(plan.merged_spec["extra"], true);

        let new_task = supervisor.get_task(&plan.new_task_id).expect("missing");
        assert_eq!(new_task.parent_task_id.as_deref(), Some(id.as_str()));
        assert_eq!(new_task.tool_name, "spawn");
        assert!(new_task.spec_snapshot.is_some());
    }

    #[test]
    fn should_reject_relaunch_for_task_without_snapshot() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("spawn", "call-no-snap", Some("api:session"));
        supervisor.mark_running(&id);

        let err = supervisor
            .relaunch_task(&id, serde_json::json!({}))
            .expect_err("should require spec snapshot");
        assert_eq!(err, RelaunchError::NoSpecSnapshot);
    }

    #[test]
    fn should_emit_task_lifecycle_cancelled_event_to_sink() {
        let dir = tempfile::TempDir::new().unwrap();
        let sink_path = dir.path().join("events.jsonl");

        let supervisor = TaskSupervisor::new();
        supervisor.attach_harness_event_sink(sink_path.to_string_lossy().to_string());

        let id = supervisor.register("spawn", "call-evt", Some("api:sess"));
        supervisor.mark_running(&id);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        supervisor.register_abort(&id, handle.abort_handle(), None, None);

        supervisor
            .cancel_task(&id, Some("kill please".into()))
            .unwrap();

        let body = std::fs::read_to_string(&sink_path).unwrap();
        let mut lines = body.lines();
        let first = lines.next().expect("at least one line emitted");
        let parsed: serde_json::Value = serde_json::from_str(first).unwrap();
        assert_eq!(
            parsed["schema"],
            crate::harness_events::HARNESS_EVENT_SCHEMA_V1
        );
        assert_eq!(parsed["kind"], "task.lifecycle.cancelled");
        assert_eq!(parsed["task_id"], id);
        assert_eq!(parsed["reason"], "kill please");
        assert_eq!(parsed["origin"], "operator");
        assert!(
            parsed
                .get("relaunched_as")
                .map(|v| v.is_null())
                .unwrap_or(true)
        );

        rt.shutdown_background();
    }

    #[test]
    fn should_cancel_original_when_relaunched_and_emit_relaunched_as() {
        let dir = tempfile::TempDir::new().unwrap();
        let sink_path = dir.path().join("events.jsonl");

        let supervisor = TaskSupervisor::new();
        supervisor.attach_harness_event_sink(sink_path.to_string_lossy().to_string());

        let id = supervisor.register("spawn", "call-r2", Some("api:sess"));
        supervisor.mark_running(&id);
        let spec = serde_json::json!({"task": "original"});

        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        supervisor.register_abort(&id, handle.abort_handle(), None, Some(spec));

        let plan = supervisor
            .relaunch_task(&id, serde_json::json!({"task": "v2"}))
            .unwrap();

        let original = supervisor.get_task(&id).unwrap();
        assert_eq!(original.status, TaskStatus::Cancelled);
        assert_eq!(
            original.cancellation_reason.as_deref(),
            Some(format!("relaunched as {}", plan.new_task_id).as_str())
        );

        let body = std::fs::read_to_string(&sink_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["origin"], "relaunch");
        assert_eq!(parsed["relaunched_as"], plan.new_task_id);

        rt.shutdown_background();
    }

    #[test]
    fn should_preserve_spec_snapshot_across_persistence_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");
        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger_path).unwrap();

        let id =
            supervisor.register_with_lineage("spawn", "call-persist", Some("api:session"), None);
        supervisor.mark_running(&id);
        let spec = serde_json::json!({"task": "persisted", "seed": 42});
        {
            let mut tasks = supervisor.tasks.lock().unwrap();
            if let Some(t) = tasks.get_mut(&id) {
                t.spec_snapshot = Some(spec.clone());
            }
        }
        supervisor.persist_snapshot_by_id(&id);

        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger_path).unwrap();
        let task = restored.get_task(&id).expect("missing");
        assert_eq!(task.spec_snapshot, Some(spec));
    }
}
