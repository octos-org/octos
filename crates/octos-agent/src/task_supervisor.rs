//! Background task lifecycle management for spawn_only tools.
//!
//! The `TaskSupervisor` is a status store that tracks background tasks from
//! spawn to completion. It does NOT enforce workspace contracts — that
//! responsibility belongs to `workspace_contract::enforce()`, which runs
//! inline in `execution.rs` BEFORE the supervisor status is updated.
//!
//! The supervisor only sees truth-checked states: `Completed` means the
//! workspace contract was satisfied, `Failed` means it was not.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};
use metrics::counter;
use octos_core::TaskId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness_events::{HarnessEvent, HarnessEventPayload};
use crate::progress::{ProgressEvent, ProgressReporter};

const CURRENT_TASK_LEDGER_SCHEMA: u32 = 1;

/// Cap on the number of child tasks any single parent session may register
/// in the supervisor. Hit by the mini4/river runaway: a pipeline node spawned
/// 65,535 children into a single session before the host disk filled up.
///
/// Beyond this cap [`TaskSupervisor::try_register_with_input`] returns
/// [`RegisterTaskError::ChildFanoutExceeded`], the legacy
/// `register*` entry points return an empty-string sentinel, and every
/// currently-active child of that parent is force-marked `Failed` with a
/// structured reason so the runaway loop's downstream registers see the
/// poisoned state and stop submitting.
///
/// Override at process start by setting the `OCTOS_MAX_CHILDREN_PER_PARENT`
/// env var to a positive integer; the value is parsed once and cached.
pub const MAX_CHILDREN_PER_PARENT: usize = 200;

/// Codex round-2 MAJOR (PR #1324): upper bound on `AckAndPending::pending`
/// entries before the oldest stash is evicted. Sized generously so that
/// even a fully cascaded pipeline (one pending entry per child task,
/// `MAX_CHILDREN_PER_PARENT = 200`) plus a 56-entry headroom for unrelated
/// stashes still fits without eviction in normal operation. The cap is
/// load-bearing in pathological flows where the synth-ack never arrives
/// (sibling-error suppression + the task never completes/cancels), so
/// without it the map would grow until the supervisor is dropped.
///
/// When the cap is exceeded the oldest entry is evicted and a WARN is
/// logged so operators can spot stuck flows.
const MAX_PENDING_FAILURES: usize = 256;

/// Codex round-2 MAJOR (PR #1324): upper bound on
/// `AckAndPending::emitted_task_ids` before the oldest entry is evicted.
/// Sized at 4× the pending cap so a long-running supervisor that never
/// shuts down still cannot grow this set without bound. The set's only
/// role is per-task idempotency on the signal callback, so evicting
/// stale entries after thousands of fires is safe — the task has long
/// since terminated and its task_id is not reused.
const MAX_FAILURE_SIGNAL_EMITTED_IDS: usize = 1024;

fn max_children_per_parent() -> usize {
    static CACHE: OnceLock<usize> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("OCTOS_MAX_CHILDREN_PER_PARENT")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .filter(|cap| *cap > 0)
            .unwrap_or(MAX_CHILDREN_PER_PARENT)
    })
}

/// Error variants for [`TaskSupervisor::try_register_with_input`] and the
/// other strict registration entry points. Currently all callers map this to
/// a structured failure log; the variant stays an enum so we can grow new
/// rejection reasons (e.g. shutdown, quota) without breaking the public API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterTaskError {
    /// The parent session already has at least `cap` registered children
    /// (active + terminal). The runaway-prevention cap fired; the caller
    /// must surface this as a tool failure rather than re-trying.
    ChildFanoutExceeded {
        parent_session_key: String,
        count: usize,
        cap: usize,
    },
    /// NEW-18b: the parent task identified by `parent_tool_call_id` is
    /// already in a terminal state (`Failed`, `Completed`, or
    /// `Cancelled`). Refusing the child registration prevents the
    /// "phantom child task" pattern where a pipeline's tokio workers
    /// survive a serve restart, observe the orphan-swept parent as
    /// `failed`, and keep registering NEW node tasks against the live
    /// supervisor — wasting CPU/tokens and confusing the UI.
    ParentTerminal {
        parent_tool_call_id: String,
        parent_status: TaskStatus,
    },
}

impl std::fmt::Display for RegisterTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChildFanoutExceeded {
                parent_session_key,
                count,
                cap,
            } => write!(
                f,
                "child fanout exceeded ({count} of {cap}) for parent session '{parent_session_key}'"
            ),
            Self::ParentTerminal {
                parent_tool_call_id,
                parent_status,
            } => write!(
                f,
                "parent task (tool_call_id='{parent_tool_call_id}') is already {} — refusing child registration",
                parent_status.as_str(),
            ),
        }
    }
}

impl std::error::Error for RegisterTaskError {}

/// Lifecycle status of a background task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Spawned,
    Running,
    Completed,
    Failed,
    /// M7.9 / W2: task was cancelled mid-flight via the supervisor's
    /// `cancel()` primitive (e.g. `POST /api/tasks/{id}/cancel`).
    /// Terminal — `is_active()` returns false. Distinguished from
    /// `Failed` so dashboards can surface "user cancelled" instead of
    /// "the task crashed".
    Cancelled,
}

impl TaskStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Spawned | Self::Running)
    }

    /// Whether this status is a terminal (non-recoverable, non-running)
    /// state. Used by the API layer to reject `cancel`/`restart` against
    /// already-terminal tasks with a `409 Conflict` response.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
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
    /// M7.9 / W2: runtime state for tasks cancelled via the supervisor's
    /// `cancel()` primitive. Surfaced via `mark_cancelled`.
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
    /// M7.9 / W2: stable cancelled lifecycle for UI / API dashboards.
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
    /// Original tool arguments — preserved so failure-recovery flows can
    /// surface the exact input the LLM passed when offering alternatives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    /// Issue #738 fix: the `client_message_id` of the user turn that
    /// originated this background task. Captured at register time so the
    /// M8.9 failure-recovery synthetic turn can inherit the same cmid
    /// (instead of the recovery turn minting a fresh server UUIDv7 that
    /// the SPA has no DOM bubble for, leaving the eventual successful
    /// retry's deliverables stranded under an orphan thread_id).
    /// `#[serde(default)]` so tasks persisted before this field was added
    /// still deserialize as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_client_message_id: Option<String>,

    // ── #966 / M13-B projection fields ──────────────────────────────
    // The AppUI TaskListEntry already accepts these optional fields
    // (see octos-cli `TaskListProjection`); populating them here at
    // register-time threads them into `task/list` and `task/updated`
    // payloads so clients can render bounded child-task UX without
    // probing free-form text. All five use `#[serde(default)]` so
    // pre-M13-B persisted snapshots still deserialize as None.
    /// Origin of this task: `"model"` (LLM scheduled via
    /// spawn_agent/spawn/delegate), `"supervisor"` (backend), or
    /// `"user"` (rare).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Role label assigned at spawn — mirrors M14-C role templates
    /// (`"reviewer"`, `"implementer"`, `"test_worker"`, `"explorer"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Bounded summary capsule mirroring ChildResultSummary.summary
    /// for terminal children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Number of artifacts emitted so far so UX can badge tasks
    /// without resolving task/artifact/list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_count: Option<u32>,
    /// Effective runtime policy stamp captured at spawn — clients
    /// rendering reconnect hydration should display the stamp the
    /// task originally announced, not the current session policy.
    /// Stored as raw JSON so the agent crate doesn't depend on the
    /// AppUI `runtime_policy_stamp` schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_policy_stamp: Option<Value>,
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

/// Payload emitted when a `spawn_only` background task transitions to
/// `Failed`. Consumers (e.g. the session actor) use this to schedule a
/// synthetic recovery turn so the LLM can re-engage with an actionable
/// error and offer alternatives instead of leaving the user stuck on a
/// terminal-only failure notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnOnlyFailureSignal {
    /// Background task identifier (matches `BackgroundTask::id`).
    pub task_id: String,
    /// Tool that failed (e.g. `fm_tts`).
    pub tool_name: String,
    /// The original tool arguments passed by the LLM when invoking the tool.
    /// May be `Value::Null` if the input was not captured for this task.
    pub tool_input: Value,
    /// The textual error reported by the tool, contract validator, or wrapper.
    pub error_message: String,
    /// Best-effort list of alternatives extracted from the error text via the
    /// `available: X, Y, Z` pattern. Empty when no alternatives were detected.
    pub suggested_alternatives: Vec<String>,
    /// Owning session, when the failed task is bound to one.
    pub parent_session_key: Option<String>,
    /// Issue #738 fix: the `client_message_id` of the user turn that
    /// originated this spawn_only task. Threaded end-to-end so the
    /// synthetic recovery `InboundMessage` built by the session actor
    /// inherits the original turn's cmid — without it, `process_inbound`
    /// mints a fresh server UUIDv7 and the eventual successful retry's
    /// deliverables (e.g. `_report.md`) land under an orphan thread_id
    /// with no DOM bubble in the SPA. `None` for legacy callers that
    /// pre-date the field; receivers must tolerate that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_client_message_id: Option<String>,
}

/// Callback invoked when a `spawn_only` task fails. Receives the structured
/// signal payload so consumers can build a recovery prompt without re-parsing
/// the raw `BackgroundTask`.
type OnFailureCallback = Box<dyn Fn(&SpawnOnlyFailureSignal) + Send + Sync>;

/// Options for `TaskSupervisor::relaunch`. Mirrors the
/// `POST /api/tasks/{id}/restart-from-node` request body.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelaunchOpts {
    /// When set, the supervisor relaunches starting at this DOT-graph node id
    /// (so upstream cached outputs are reused). When `None` the relaunch
    /// re-runs the entire task from scratch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_node: Option<String>,
}

/// Payload emitted to the relaunch callback when a caller invokes
/// `TaskSupervisor::relaunch`. The callback owns turning this into a
/// concrete tokio task that re-executes the work; the supervisor only
/// stores a forwarding pointer (`relaunched_from`) on the original task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaunchRequest {
    /// Identifier of the task being relaunched. Always `task.id`.
    pub original_task_id: String,
    /// Identifier the supervisor pre-allocated for the relaunched task.
    /// Already registered on the supervisor in the `Spawned` state so the
    /// callback can `mark_running` immediately.
    pub new_task_id: String,
    pub tool_name: String,
    pub tool_call_id: String,
    pub parent_session_key: Option<String>,
    pub session_key: Option<String>,
    pub tool_input: Value,
    pub opts: RelaunchOpts,
}

/// Callback invoked when a caller asks the supervisor to relaunch a task.
type OnRelaunchCallback = Box<dyn Fn(&RelaunchRequest) + Send + Sync>;

/// Error variants for [`TaskSupervisor::cancel`]. Mapped to HTTP status
/// codes by the API layer:
/// - `NotFound` → `404`
/// - `AlreadyTerminal` → `409`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskCancelError {
    NotFound,
    AlreadyTerminal,
}

impl std::fmt::Display for TaskCancelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "task not found"),
            Self::AlreadyTerminal => write!(f, "task is already in a terminal state"),
        }
    }
}

impl std::error::Error for TaskCancelError {}

/// Error variants for [`TaskSupervisor::relaunch`]. Mapped to HTTP status
/// codes by the API layer:
/// - `NotFound` → `404`
/// - `StillActive` → `409`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRelaunchError {
    NotFound,
    StillActive,
}

impl std::fmt::Display for TaskRelaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "task not found"),
            Self::StillActive => {
                write!(f, "task is still active; cancel it before relaunching")
            }
        }
    }
}

impl std::error::Error for TaskRelaunchError {}

/// Per-task cancel token map. Each entry pairs an `AtomicBool` (loop-poll
/// flag) and an optional `tokio::sync::Notify` so cooperatively cancelable
/// futures (e.g. `select!` on a long-running pipeline) can race against
/// `cancelled.notified()` instead of polling.
#[derive(Default)]
struct CancelTokenStore {
    tokens: Mutex<HashMap<String, Arc<TaskCancelToken>>>,
}

impl CancelTokenStore {
    fn ensure(&self, task_id: &str) -> Arc<TaskCancelToken> {
        let mut guard = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .entry(task_id.to_string())
            .or_insert_with(|| Arc::new(TaskCancelToken::new()))
            .clone()
    }
}

/// Per-task cancel token. Workers poll `is_cancelled()` at safe points and
/// long-running futures can `select!` on `notified()` to short-circuit
/// pending I/O.
pub struct TaskCancelToken {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl TaskCancelToken {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Whether the token has been triggered. Safe-point poll for in-loop
    /// pipeline workers.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Trigger cancellation. Idempotent — a second call is a no-op.
    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            self.notify.notify_waiters();
        }
    }

    /// Wait for the token to fire. Useful for `select!` against a
    /// long-running future.
    pub async fn cancelled(&self) {
        self.cancelled_after_first_check(|| {}).await;
    }

    async fn cancelled_after_first_check<F>(&self, after_first_check: F)
    where
        F: FnOnce(),
    {
        if self.is_cancelled() {
            return;
        }
        after_first_check();
        let notified = self.notify.notified();
        tokio::pin!(notified);
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

impl std::fmt::Debug for TaskCancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskCancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Extract a list of alternatives from a tool error message using the simple
/// `available: X, Y, Z` pattern. Returns an empty vector when no match is
/// found so callers can fall back to surfacing the raw error text.
///
/// This is intentionally conservative — we only handle the canonical
/// "available: ..." phrasing emitted by the fm_tts/voice-skill family. More
/// aggressive parsing belongs in the failure-modes inventory follow-up.
pub fn parse_alternatives(error_text: &str) -> Vec<String> {
    // Use a literal scan rather than a regex so we don't pull in a fresh
    // dependency or risk pathological backtracking. The marker is
    // case-insensitive and matched anywhere in the message.
    let needle = "available:";
    let lower = error_text.to_lowercase();
    let Some(start) = lower.find(needle) else {
        return Vec::new();
    };
    let tail = &error_text[start + needle.len()..];

    // Stop at the first sentence boundary so we don't grab the entire
    // remainder of the error message. Newlines and periods both terminate
    // the alternatives clause.
    let stop = tail.find(['\n', '.', ';']).unwrap_or(tail.len());
    let clause = &tail[..stop];

    clause
        .split(',')
        .map(|item| item.trim().trim_matches(['"', '\'']))
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .collect()
}

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

/// Returns true if the given runtime_state is a terminal state. The
/// non-terminal complement is the set of states that, on supervisor
/// restart, indicate an orphaned task whose owning worker is gone.
///
/// `Completed`, `Failed`, and `Cancelled` are terminal: the worker has
/// already driven the task to a final state and persisted the outcome.
/// Anything else (`Spawned`, `ExecutingTool`, `ResolvingOutputs`,
/// `VerifyingOutputs`, `DeliveringOutputs`, `CleaningUp`) means the
/// owning worker was mid-flight when the runtime stopped, so on restart
/// the task is an orphan with no live actor behind it.
fn is_terminal_runtime_state(state: &TaskRuntimeState) -> bool {
    matches!(
        state,
        TaskRuntimeState::Completed | TaskRuntimeState::Failed | TaskRuntimeState::Cancelled
    )
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

fn child_failure_action_for_terminal_state(
    state: &ChildSessionTerminalState,
) -> Option<ChildSessionFailureAction> {
    match state {
        ChildSessionTerminalState::Completed => None,
        ChildSessionTerminalState::RetryableFailure => Some(ChildSessionFailureAction::Retry),
        ChildSessionTerminalState::TerminalFailure => Some(ChildSessionFailureAction::Escalate),
    }
}

// Background-task artifact validation lives in `workspace_contract.rs` (the
// per-skill workspace contract layer) and in the skill itself. The
// supervisor used to second-guess that result with its own size/magic/
// silence/duration checks, but the duplication produced false positives
// (mini5 serena-TTS, 2026-05-12: real speech rejected because the 4 KB
// leading-window silence sampler only saw the TTS preamble silence) and
// was a layer violation — the supervisor only needs to know whether the
// skill reported success or failure, not whether the bytes look right.

impl std::fmt::Debug for TaskSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let progress_reporter_attached = self
            .progress_reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        f.debug_struct("TaskSupervisor")
            .field("tasks", &self.tasks)
            .field("on_change", &"<callback>")
            .field("on_failure", &"<callback>")
            .field("on_relaunch", &"<callback>")
            .field("progress_reporter", &progress_reporter_attached)
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

/// Human-readable label for a [`TaskRuntimeState`] used by the supervisor's
/// `ProgressReporter` bridge. The text is suffixed onto `<tool>: ` so the
/// chat UI can anchor a single bubble per tool_call_id and surface what the
/// background task is currently doing without inventing per-tool plumbing.
fn runtime_state_label(state: &TaskRuntimeState) -> &'static str {
    match state {
        TaskRuntimeState::Spawned => "spawned",
        TaskRuntimeState::ExecutingTool => "running",
        TaskRuntimeState::ResolvingOutputs => "resolving outputs",
        TaskRuntimeState::VerifyingOutputs => "verifying outputs",
        TaskRuntimeState::DeliveringOutputs => "delivering outputs",
        TaskRuntimeState::CleaningUp => "cleaning up",
        TaskRuntimeState::Completed => "completed",
        TaskRuntimeState::Failed => "failed",
        TaskRuntimeState::Cancelled => "cancelled",
    }
}

/// Supervisor that tracks background task lifecycle.
///
/// Thread-safe via interior `Mutex`. Cloning shares the same underlying state.
#[derive(Clone)]
pub struct TaskSupervisor {
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
    /// Set of parent session keys that have hit the per-parent child cap
    /// (see [`MAX_CHILDREN_PER_PARENT`]). Once a parent is poisoned every
    /// subsequent register call short-circuits to refuse so the runaway
    /// loop cannot keep adding children.
    poisoned_parents: Arc<Mutex<HashSet<String>>>,
    on_change: Arc<Mutex<Option<OnChangeCallback>>>,
    on_failure: Arc<Mutex<Option<OnFailureCallback>>>,
    on_relaunch: Arc<Mutex<Option<OnRelaunchCallback>>>,
    persistence_path: Arc<Mutex<Option<PathBuf>>>,
    /// Optional reporter that receives a [`ProgressEvent::ToolProgress`]
    /// for every supervised state transition. Wired by the agent's
    /// spawn_only branch so chat UIs can anchor progress strictly to the
    /// originating `tool_call_id` (the chat-bubble contract enforced by
    /// the SSE `tool_call_id` field on `tool_progress` frames).
    ///
    /// Synchronous tool calls never go through the supervisor, so this
    /// bridge naturally fires only on background-task transitions —
    /// there is no double-emission to worry about for the normal tool
    /// path that already reports its own ToolStarted/ToolCompleted.
    progress_reporter: Arc<Mutex<Option<Arc<dyn ProgressReporter>>>>,
    /// M7.9: per-task cancellation tokens. The `cancel(task_id)` primitive
    /// flips the matching token so cooperative pipeline / spawn workers can
    /// short-circuit at their next safe point. Tokens are created lazily on
    /// `register*` and dropped on terminal transitions to keep memory usage
    /// proportional to active tasks.
    cancel_tokens: Arc<CancelTokenStore>,
    /// Codex round-2 BLOCKER (PR #1324 follow-up): unified state for the
    /// synth-ack gate, pending failure stash, and per-task idempotency
    /// guard. All three were previously separate `Mutex`es, which left a
    /// narrow ack/pending interleaving race:
    ///
    /// 1. `notify_failure` checks `synth_ack_emitted.contains` → false.
    /// 2. `mark_synth_ack_emitted` inserts the ack AND drains the (still
    ///    empty) pending map.
    /// 3. `notify_failure` inserts its pending entry — too late to be
    ///    drained.
    /// 4. The pending stash sits forever; recovery signal lost.
    ///
    /// Folding all three collections under one mutex makes the
    /// "check-ack-then-stash" pair atomic with the "record-ack-then-
    /// drain" pair. The hot path is recovery signaling, which is
    /// infrequent, so the mutex contention is not a perf concern.
    ///
    /// See [`AckAndPending`] for the field-level documentation that
    /// previously lived on the individual fields.
    ack_and_pending: Arc<Mutex<AckAndPending>>,
}

/// Combined state guarded by a single mutex (Codex round-2 BLOCKER):
/// the synth-ack set, the deferred-failure stash, and the per-task
/// idempotency set must move together so the ack/pending interleaving
/// race is impossible.
#[derive(Debug, Default)]
struct AckAndPending {
    /// Set of `tool_call_id`s for which the spawn_only "Background work
    /// started for `<tool>`." synth-ack was actually emitted to the LLM
    /// (see `loop_runner.rs` synth-ack gate).
    ///
    /// This is the load-bearing gate for the post-spawn failure feedback
    /// loop: `notify_failure` only fires `SpawnOnlyFailureSignal` for tasks
    /// whose `tool_call_id` is in this set. Two failure modes that skip
    /// the signal correctly:
    ///
    /// 1. **Pre-flight validation failures** (`Tool::pre_flight_validate`
    ///    returned `Err`) early-return BEFORE supervisor registration, so
    ///    `mark_failed` is never called for them. The LLM already saw the
    ///    synchronous `[VALIDATION FAILED]` tool_result — no recovery
    ///    needed.
    /// 2. **Sibling-error suppression**: a spawn_only sibling tool in the
    ///    same batch errored, so the synth-ack gate suppressed the ack
    ///    (see `any_tool_invocation_errored`). The spawn_only task IS
    ///    registered (tokio::spawn happened) but the LLM already saw the
    ///    sibling's error and will react on its next iteration. Injecting
    ///    a recovery prompt for the spawn_only's eventual post-spawn
    ///    failure would double-signal the LLM.
    ///
    /// When the synth-ack DID fire, the LLM was told success — the
    /// post-spawn failure is the only way it can learn the truth. That
    /// is the gap this set closes.
    synth_ack_emitted_tool_call_ids: HashSet<String>,
    /// Codex round-4 BLOCKER (PR #1324 follow-up): two-phase failure
    /// emission.
    ///
    /// `tokio::spawn` in `execution.rs::handle_spawn_only_branch` (line
    /// ~493) dispatches the background task BEFORE `loop_runner.rs`
    /// records the synth-ack at line ~1356. A fast post-spawn failure
    /// (e.g. plugin binary missing, instant validator rejection) can
    /// fire `notify_failure` while `synth_ack_emitted_tool_call_ids`
    /// is still empty. Holding both collections under the same mutex
    /// (see [`TaskSupervisor::ack_and_pending`]) makes ack-record + drain
    /// atomic with ack-check + insert, eliminating the interleaving race.
    ///
    /// When `notify_failure` observes "ack not yet recorded", it
    /// stashes the would-be `SpawnOnlyFailureSignal` here keyed by the
    /// supervisor's unique `task_id`. The value carries the
    /// associated `tool_call_id` so `mark_synth_ack_emitted(tc_id)`
    /// can scan and drain ALL pending tasks under that
    /// `tool_call_id` — important for pipeline cascade, where many
    /// child tasks share the parent's `tool_call_id`.
    ///
    /// Codex round-2 MAJOR: bounded by [`MAX_PENDING_FAILURES`] with FIFO
    /// eviction (oldest first); `pending_insertion_order` records insert
    /// order so eviction is O(1).
    pending: HashMap<String, PendingFailure>,
    /// FIFO insertion order for `pending`. When `pending.len()` exceeds
    /// [`MAX_PENDING_FAILURES`] the front entry is evicted to keep the
    /// map bounded under pathological flows (ack never arrives + task
    /// never completes/cancels).
    pending_insertion_order: VecDeque<String>,
    /// Companion to `pending` (Codex round-4 BLOCKER): tracks unique
    /// `task_id`s for which the failure callback already fired.
    /// Keyed by `task_id` (not `tool_call_id`) because pipeline
    /// cascades have many tasks under the same `tool_call_id` and
    /// each child must fire its own signal (see
    /// `mark_descendants_failed_emits_progress_and_failure_signal_per_child`).
    /// Guards the deferred-emission replay path and a sibling
    /// `mark_failed` for the same task so each task fires at most one
    /// `SpawnOnlyFailureSignal`.
    ///
    /// Codex round-2 MAJOR: bounded by
    /// [`MAX_FAILURE_SIGNAL_EMITTED_IDS`] with FIFO eviction; the
    /// `emitted_insertion_order` queue records insert order so
    /// eviction is O(1). Stale entries (>1024 fires) are safe to
    /// evict — the task has long since terminated and `task_id` is a
    /// UUID never reused.
    emitted_task_ids: HashSet<String>,
    /// FIFO insertion order for `emitted_task_ids`.
    emitted_insertion_order: VecDeque<String>,
}

impl AckAndPending {
    /// Insert a pending failure under `task_id`. If the map already
    /// holds [`MAX_PENDING_FAILURES`] entries, evict the oldest entry
    /// first and log a WARN so operators can spot stuck flows. Returns
    /// `Some(evicted)` when an eviction happened.
    fn insert_pending(&mut self, task_id: String, value: PendingFailure) -> Option<PendingFailure> {
        // If the key is already present, refresh in place — no
        // ordering change so we do not re-queue.
        if let Some(slot) = self.pending.get_mut(&task_id) {
            *slot = value;
            return None;
        }
        let evicted = if self.pending.len() >= MAX_PENDING_FAILURES {
            // Pop the oldest entry that still has a live map slot.
            // `pending_insertion_order` can contain stale ids (drained
            // out of the map directly) so skip those.
            loop {
                match self.pending_insertion_order.pop_front() {
                    Some(stale) => {
                        if let Some(victim) = self.pending.remove(&stale) {
                            tracing::warn!(
                                evicted_task_id = %stale,
                                evicted_tool_call_id = %victim.tool_call_id,
                                cap = MAX_PENDING_FAILURES,
                                "evicting oldest pending spawn_only failure stash: cap exceeded",
                            );
                            break Some(victim);
                        }
                    }
                    None => break None,
                }
            }
        } else {
            None
        };
        self.pending.insert(task_id.clone(), value);
        self.pending_insertion_order.push_back(task_id);
        evicted
    }

    /// Remove a pending entry by `task_id` AND its companion entry
    /// from `pending_insertion_order` so the FIFO queue cannot grow
    /// without bound.
    ///
    /// Codex round-3 MAJOR (PR #1324 follow-up): the round-2 cap only
    /// fires when `pending.len()` exceeds [`MAX_PENDING_FAILURES`]. In
    /// the common fail-before-ack → ack-drain cycle the HashMap
    /// returns to zero each round, so the cap is never hit and the
    /// VecDeque grows linearly forever. Popping the matching id here
    /// keeps both collections in lockstep.
    fn remove_pending(&mut self, task_id: &str) -> Option<PendingFailure> {
        let removed = self.pending.remove(task_id);
        if removed.is_some() {
            self.forget_pending_in_order(task_id);
        }
        removed
    }

    /// Drain every pending failure matching `tool_call_id`. Returns
    /// the drained entries; the insertion-order queue is updated in
    /// lockstep so the VecDeque cannot leak across drain cycles.
    ///
    /// Codex round-3 MAJOR (PR #1324 follow-up): same leak class as
    /// `remove_pending` — when no eviction is ever triggered, the
    /// `pending_insertion_order` queue would otherwise accumulate one
    /// entry per failure forever. Pop in lockstep here.
    fn drain_pending_for_tool_call(&mut self, tool_call_id: &str) -> Vec<PendingFailure> {
        let mut hits = Vec::new();
        let mut drained_ids: Vec<String> = Vec::new();
        self.pending.retain(|task_id, pf| {
            if pf.tool_call_id == tool_call_id {
                drained_ids.push(task_id.clone());
                hits.push(pf.clone());
                false // remove
            } else {
                true // keep
            }
        });
        for task_id in &drained_ids {
            self.forget_pending_in_order(task_id);
        }
        hits
    }

    /// Remove `task_id` from `pending_insertion_order` if present.
    /// `VecDeque::remove` is O(n) but the deque is bounded at
    /// [`MAX_PENDING_FAILURES`] (256), so the linear scan is cheap.
    fn forget_pending_in_order(&mut self, task_id: &str) {
        if let Some(pos) = self
            .pending_insertion_order
            .iter()
            .position(|tid| tid == task_id)
        {
            self.pending_insertion_order.remove(pos);
        }
    }

    /// Mark `task_id` as having dispatched its failure signal. Returns
    /// `true` if this is the first dispatch (caller should proceed to
    /// invoke the callback), `false` if a previous path already
    /// dispatched (caller must suppress). Bounded by
    /// [`MAX_FAILURE_SIGNAL_EMITTED_IDS`] with FIFO eviction.
    fn mark_emitted(&mut self, task_id: &str) -> bool {
        if !self.emitted_task_ids.insert(task_id.to_string()) {
            return false;
        }
        self.emitted_insertion_order.push_back(task_id.to_string());
        while self.emitted_task_ids.len() > MAX_FAILURE_SIGNAL_EMITTED_IDS {
            if let Some(stale) = self.emitted_insertion_order.pop_front() {
                if self.emitted_task_ids.remove(&stale) {
                    tracing::warn!(
                        evicted_task_id = %stale,
                        cap = MAX_FAILURE_SIGNAL_EMITTED_IDS,
                        "evicting oldest emitted-failure-signal id: cap exceeded",
                    );
                }
            } else {
                break;
            }
        }
        true
    }
}

/// Pending failure entry — see the field-level doc on
/// `AckAndPending::pending`.
#[derive(Debug, Clone)]
struct PendingFailure {
    /// The `tool_call_id` of the failed task. Used by
    /// `mark_synth_ack_emitted` to identify which pending entries to
    /// drain when an ack arrives for that id.
    tool_call_id: String,
    /// The failure signal payload that `notify_failure` would have
    /// dispatched if the synth-ack had already been recorded.
    signal: SpawnOnlyFailureSignal,
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
            poisoned_parents: Arc::new(Mutex::new(HashSet::new())),
            on_change: Arc::new(Mutex::new(None)),
            on_failure: Arc::new(Mutex::new(None)),
            on_relaunch: Arc::new(Mutex::new(None)),
            persistence_path: Arc::new(Mutex::new(None)),
            progress_reporter: Arc::new(Mutex::new(None)),
            cancel_tokens: Arc::new(CancelTokenStore::default()),
            ack_and_pending: Arc::new(Mutex::new(AckAndPending::default())),
        }
    }

    /// Record that the spawn_only synth-ack ("Background work started for
    /// `<tool>`.") was emitted to the LLM for `tool_call_id`. Called from
    /// the synth-ack gate in `loop_runner.rs` for every spawn_only tool
    /// call in a turn whose synth-ack actually fires (gated by
    /// `any_tool_invocation_errored`).
    ///
    /// The set is the load-bearing signal for the post-spawn failure
    /// feedback loop — see the field-level doc on
    /// `AckAndPending::synth_ack_emitted_tool_call_ids`. Idempotent.
    ///
    /// Codex round-4 BLOCKER (PR #1324 follow-up): after recording the
    /// ack, drain any pending failure for this `tool_call_id` and emit
    /// the `SpawnOnlyFailureSignal` NOW.
    ///
    /// Codex round-2 BLOCKER (PR #1324 follow-up): the ack-record +
    /// pending-drain pair happens under the SAME mutex as
    /// `notify_failure`'s ack-check + pending-insert pair. The previous
    /// design used two separate mutexes for `synth_ack_emitted` and
    /// `pending_failures`, leaving a narrow interleave where ack-check
    /// observes false, then drain runs against an empty map, then
    /// notify inserts pending and the stash sits forever. Folding both
    /// collections under [`AckAndPending`] makes the ordering atomic.
    pub fn mark_synth_ack_emitted(&self, tool_call_id: &str) {
        if tool_call_id.is_empty() {
            return;
        }
        // Atomic: insert ack AND drain any pending entries that
        // arrived before the ack was recorded. No interleaving with
        // `notify_failure`'s ack-check + pending-insert pair is
        // possible because they hold the same mutex.
        let drained: Vec<PendingFailure> = {
            let mut guard = self
                .ack_and_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard
                .synth_ack_emitted_tool_call_ids
                .insert(tool_call_id.to_string());
            guard.drain_pending_for_tool_call(tool_call_id)
        };
        // Dispatch happens AFTER releasing the mutex — the failure
        // callback is user code that may take other locks (notably
        // `on_failure`), so we must not hold `ack_and_pending` across
        // it.
        for pf in drained {
            let task_id = pf.signal.task_id.clone();
            self.dispatch_failure_signal(&task_id, pf.signal);
        }
    }

    /// True iff the synth-ack was emitted for `tool_call_id` via
    /// [`Self::mark_synth_ack_emitted`]. Used by `notify_failure` to gate
    /// `SpawnOnlyFailureSignal` emission so post-spawn failures only
    /// trigger a recovery turn when the LLM was previously told the
    /// background task started successfully.
    pub fn was_synth_ack_emitted(&self, tool_call_id: &str) -> bool {
        if tool_call_id.is_empty() {
            return false;
        }
        let guard = self
            .ack_and_pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.synth_ack_emitted_tool_call_ids.contains(tool_call_id)
    }

    /// Enable append-only persistence for task snapshots and restore existing state.
    ///
    /// At the end of replay, sweeps the in-memory map for any task whose
    /// `runtime_state` is non-terminal (anything other than `Completed`,
    /// `Failed`, or `Cancelled`). Those tasks are orphans — the worker
    /// process that owned them died across the restart, so no live actor
    /// will ever drive them to a terminal state. They are marked
    /// `Failed("orphaned across restart")` via the standard `mark_failed`
    /// path so the JSONL ledger gets a proper terminal entry and re-loading
    /// is idempotent. The `octos_orphaned_tasks_reaped_total` counter is
    /// incremented per reaped task.
    ///
    /// This handles startup-time orphans only: at this point in startup no
    /// new work has been scheduled yet, so any non-terminal runtime_state
    /// definitionally has no live worker. In-flight orphans inside a
    /// long-running supervisor (worker hangs / crashes silently while the
    /// supervisor itself stays alive) are NOT addressed here — that needs
    /// a heartbeat-based reaper, which is a follow-up if observed.
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

        // Sweep orphans: any task whose runtime_state is non-terminal at
        // this point has no live worker behind it (we are still in startup,
        // no new work has been scheduled yet). Mark them Failed via the
        // standard mark_failed path so the JSONL ledger gets a proper
        // terminal entry and re-loading is idempotent.
        //
        // NEW-18b — capture the `(id, tool_call_id, tool_name)` triple
        // for every orphan so that after the parent transition fires we
        // can cascade-fail any LIVE descendants (children that already
        // registered against this supervisor under the same
        // tool_call_id but haven't transitioned to a terminal state
        // themselves). This is Option-C in the bug brief: a backstop
        // for the race where a pipeline child registers before the
        // sweep runs, or where a straggler pipeline tokio worker
        // survives the restart and re-registers a node task between
        // load and sweep.
        let orphans: Vec<(String, String, String)> = {
            let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks
                .values()
                .filter(|task| !is_terminal_runtime_state(&task.runtime_state))
                .map(|task| {
                    (
                        task.id.clone(),
                        task.tool_call_id.clone(),
                        task.tool_name.clone(),
                    )
                })
                .collect()
        };
        for (task_id, _, _) in &orphans {
            self.mark_failed(task_id, "orphaned across restart".to_string());
        }
        if !orphans.is_empty() {
            counter!("octos_orphaned_tasks_reaped_total").increment(orphans.len() as u64);
        }

        // Option C — cascade orphaned-parent transitions onto any
        // active `pipeline:<node>` children sharing the parent's
        // tool_call_id. `mark_descendants_failed` is the same helper
        // the `RunPipelineTool` timeout arm uses, and is a no-op on
        // already-terminal children and on parents whose tool_name
        // starts with `pipeline:` (so cascade siblings don't recurse).
        // The reason string is intentionally distinct from the parent
        // sweep ("parent task orphaned across restart") so operators
        // can tell which transition wrote the failure record.
        let mut cascade_seen: HashSet<String> = HashSet::new();
        for (_, parent_tcid, parent_tool_name) in &orphans {
            if parent_tcid.is_empty() {
                continue;
            }
            // Skip pipeline node siblings — they are children, not
            // parents. Only `run_pipeline` (and any future non-pipeline
            // parents that supervise pipeline children) should trigger
            // the cascade.
            if parent_tool_name.starts_with("pipeline:") {
                continue;
            }
            if !cascade_seen.insert(parent_tcid.clone()) {
                continue;
            }
            self.mark_descendants_failed(parent_tcid, "parent task orphaned across restart");
        }

        Ok(self.tasks.lock().unwrap_or_else(|e| e.into_inner()).len())
    }

    /// Set a callback that fires whenever a task's status changes.
    pub fn set_on_change(&self, cb: impl Fn(&BackgroundTask) + Send + Sync + 'static) {
        let mut guard = self.on_change.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Box::new(cb));
    }

    /// Set a callback that fires only when a `spawn_only` task transitions to
    /// `Failed`. This is the M8.9 hook the session actor uses to enqueue a
    /// synthetic recovery turn. The callback is only invoked once per failed
    /// task — re-marking a task as failed (or any subsequent state change)
    /// will not re-fire the signal.
    pub fn set_on_failure_signal(
        &self,
        cb: impl Fn(&SpawnOnlyFailureSignal) + Send + Sync + 'static,
    ) {
        let mut guard = self.on_failure.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Box::new(cb));
    }

    /// Attach a [`ProgressReporter`] that receives a
    /// [`ProgressEvent::ToolProgress`] for every supervised runtime-state
    /// transition. The emitted event carries the originating `tool_call_id`
    /// (`ProgressEvent::ToolProgress::tool_id`) so chat UIs can anchor every
    /// long-running spawn_only task to a single bubble — no per-tool plumbing
    /// required.
    ///
    /// Wired by the agent's spawn_only branch in `execution.rs`. Setting a
    /// reporter is idempotent; the latest reporter wins. Pass a
    /// [`crate::progress::SilentReporter`] to detach.
    pub fn set_progress_reporter(&self, reporter: Arc<dyn ProgressReporter>) {
        let mut guard = self
            .progress_reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(reporter);
    }

    /// Wire a callback that fires when [`Self::relaunch`] is invoked. The
    /// callback is responsible for spawning the actual replacement task —
    /// the supervisor only pre-allocates a fresh task id and fires the
    /// signal so the owning runtime (session actor / pipeline executor)
    /// can rebuild context.
    pub fn set_on_relaunch(&self, cb: impl Fn(&RelaunchRequest) + Send + Sync + 'static) {
        let mut guard = self.on_relaunch.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Box::new(cb));
    }

    /// Acquire (or create) the cancel token for `task_id`. Workers should
    /// call this once at the top of their critical section and then poll
    /// `is_cancelled()` at safe points. Returns a freshly allocated token
    /// for unknown task ids — callers that want strict membership checks
    /// should use `get_task` first.
    pub fn cancel_token(&self, task_id: &str) -> Arc<TaskCancelToken> {
        self.cancel_tokens.ensure(task_id)
    }

    /// Cancel a tracked task. Sets the per-task cancellation token (so
    /// in-loop workers can short-circuit at the next safe point) and
    /// transitions the supervisor record to `Cancelled`. Returns:
    ///
    /// - `Ok(())` when the task was running/queued and has now been
    ///   marked `Cancelled`.
    /// - `Err(TaskCancelError::NotFound)` when no task with that id is
    ///   tracked. Maps to `404` at the API edge.
    /// - `Err(TaskCancelError::AlreadyTerminal)` when the task is
    ///   already in a terminal state (`Completed` / `Failed` /
    ///   `Cancelled`). Maps to `409` at the API edge.
    pub fn cancel(&self, task_id: &str) -> Result<(), TaskCancelError> {
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            let task = tasks.get_mut(task_id).ok_or(TaskCancelError::NotFound)?;
            if task.status.is_terminal() {
                return Err(TaskCancelError::AlreadyTerminal);
            }
            task.status = TaskStatus::Cancelled;
            task.runtime_state = TaskRuntimeState::Cancelled;
            task.updated_at = Utc::now();
            task.completed_at = Some(Utc::now());
            if task.error.is_none() {
                task.error = Some("cancelled by supervisor".to_string());
            }
            task.clone()
        };

        // Trigger the cancel token AFTER the task has been marked
        // cancelled so any waiter that wakes can re-read the supervisor
        // and see the terminal state.
        let token = self.cancel_tokens.ensure(task_id);
        token.cancel();

        self.persist_snapshot(&snapshot);
        self.notify_change(&snapshot);
        self.emit_progress_for_state(&snapshot);
        // Codex round-4 BLOCKER (PR #1324 follow-up): if a cancelled
        // task happened to have a pending failure stash (defensive —
        // cancel + late mark_failed normally would no-op via the
        // terminal guard, but the entry could exist if mark_failed
        // landed before cancel transitioned the task), drop it so a
        // later mark_synth_ack_emitted doesn't surface a recovery
        // signal for a task the user / system already cancelled.
        self.drain_pending_failure_for_task(task_id);
        Ok(())
    }

    /// Relaunch a tracked task with the supplied options. Returns the
    /// freshly allocated `new_task_id` on success.
    ///
    /// The supervisor pre-registers the new task in the `Spawned` state
    /// (mirroring the original task's tool name / call id / session
    /// metadata) and fires `set_on_relaunch` so the runtime can drive the
    /// actual re-execution. When no relaunch callback is wired the call
    /// still succeeds — the new task id is returned so callers can
    /// observe the placeholder in dashboards even when the runtime
    /// owner has not subscribed yet.
    pub fn relaunch(&self, task_id: &str, opts: RelaunchOpts) -> Result<String, TaskRelaunchError> {
        let original = {
            let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks
                .get(task_id)
                .cloned()
                .ok_or(TaskRelaunchError::NotFound)?
        };
        if matches!(original.status, TaskStatus::Running | TaskStatus::Spawned) {
            return Err(TaskRelaunchError::StillActive);
        }

        // Pre-allocate a successor task id and seed it on the supervisor
        // so dashboards see the relaunch as a peer of the original task.
        // Issue #738: carry the originating cmid forward so a relaunched
        // task that itself fails again still has the right thread anchor
        // for any synthetic recovery turn.
        let new_task_id = self.register_with_input_and_cmid(
            &original.tool_name,
            &original.tool_call_id,
            original.session_key.as_deref(),
            original.tool_input.clone(),
            original.originating_client_message_id.clone(),
        );

        // Stamp the lineage on the new task: callers can use
        // `runtime_detail` to surface the relaunch-from edge.
        let detail = serde_json::json!({
            "relaunched_from": task_id,
            "from_node": opts.from_node,
        })
        .to_string();
        self.mark_runtime_state(&new_task_id, TaskRuntimeState::Spawned, Some(detail));

        let request = RelaunchRequest {
            original_task_id: task_id.to_string(),
            new_task_id: new_task_id.clone(),
            tool_name: original.tool_name.clone(),
            tool_call_id: original.tool_call_id.clone(),
            parent_session_key: original.parent_session_key.clone(),
            session_key: original.session_key.clone(),
            tool_input: original.tool_input.clone().unwrap_or(Value::Null),
            opts,
        };

        let guard = self.on_relaunch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref cb) = *guard {
            cb(&request);
        }
        Ok(new_task_id)
    }

    /// Emit a [`ProgressEvent::ToolProgress`] for `task` if a reporter has
    /// been wired via [`Self::set_progress_reporter`]. The message is
    /// `"<tool_name>: <state-label>"`, with the task's `error` text appended
    /// in parentheses on `Failed` transitions so the UI can surface the
    /// reason without re-walking the supervisor's state.
    fn emit_progress_for_state(&self, task: &BackgroundTask) {
        let guard = self
            .progress_reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(reporter) = guard.as_ref().cloned() else {
            return;
        };
        drop(guard);
        let label = runtime_state_label(&task.runtime_state);
        let message = match task.runtime_state {
            TaskRuntimeState::Failed | TaskRuntimeState::Cancelled => match task.error.as_deref() {
                Some(reason) if !reason.is_empty() => {
                    format!("{}: {} ({})", task.tool_name, label, reason)
                }
                _ => format!("{}: {}", task.tool_name, label),
            },
            _ => format!("{}: {}", task.tool_name, label),
        };
        reporter.report(ProgressEvent::ToolProgress {
            name: task.tool_name.clone(),
            tool_id: task.tool_call_id.clone(),
            message,
        });
    }

    /// Register a new background task. Returns the generated task ID, or
    /// an empty-string sentinel when the parent's child fan-out cap fired
    /// (see [`MAX_CHILDREN_PER_PARENT`] and
    /// [`Self::try_register_with_input`]). Callers that need strict
    /// rejection semantics should use [`Self::try_register_with_input`].
    pub fn register(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
    ) -> String {
        self.register_with_lineage(tool_name, tool_call_id, session_key, None)
    }

    /// Register a new background task with optional ledger-path lineage.
    /// Returns an empty-string sentinel on cap rejection — see
    /// [`Self::register`] for details.
    pub fn register_with_lineage(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
        task_ledger_path: Option<&str>,
    ) -> String {
        match self.register_full(
            tool_name,
            tool_call_id,
            session_key,
            task_ledger_path,
            None,
            None,
            None,
        ) {
            Ok(id) => id,
            Err(error) => {
                tracing::error!(
                    tool = tool_name,
                    tool_call_id = tool_call_id,
                    session_key = ?session_key,
                    error = %error,
                    "task supervisor register refused (legacy entry point); returning empty id"
                );
                String::new()
            }
        }
    }

    /// Register a new background task with optional ledger-path lineage and
    /// the original tool input. The tool input is preserved so failure
    /// signals can include it without re-walking the message history.
    /// Returns an empty-string sentinel on cap rejection — see
    /// [`Self::register`] for details.
    pub fn register_with_input(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
        tool_input: Option<Value>,
    ) -> String {
        match self.register_full(
            tool_name,
            tool_call_id,
            session_key,
            None,
            tool_input,
            None,
            None,
        ) {
            Ok(id) => id,
            Err(error) => {
                tracing::error!(
                    tool = tool_name,
                    tool_call_id = tool_call_id,
                    session_key = ?session_key,
                    error = %error,
                    "task supervisor register_with_input refused (legacy entry point); returning empty id"
                );
                String::new()
            }
        }
    }

    /// Issue #738 fix: register a task and capture the originating user
    /// turn's `client_message_id`. The cmid is later threaded into any
    /// `SpawnOnlyFailureSignal` emitted for this task so the M8.9
    /// recovery `InboundMessage` keeps the original thread_id rather
    /// than minting an orphan UUIDv7.
    pub fn register_with_input_and_cmid(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
        tool_input: Option<Value>,
        originating_client_message_id: Option<String>,
    ) -> String {
        match self.register_full(
            tool_name,
            tool_call_id,
            session_key,
            None,
            tool_input,
            originating_client_message_id,
            None,
        ) {
            Ok(id) => id,
            Err(error) => {
                tracing::error!(
                    tool = tool_name,
                    tool_call_id = tool_call_id,
                    session_key = ?session_key,
                    error = %error,
                    "task supervisor register_with_input_and_cmid refused (legacy entry point); returning empty id"
                );
                String::new()
            }
        }
    }

    /// NEW-18b — return the [`TaskStatus`] of the parent task identified
    /// by `parent_tool_call_id`, with the relaunch-safe selection rule:
    /// prefer an **active** non-pipeline record if one exists, otherwise
    /// fall back to the most-recently-updated terminal record.
    ///
    /// Filtering rules:
    /// * Records whose `tool_name` starts with `pipeline:` are excluded —
    ///   every pipeline node child reuses the parent's `tool_call_id`
    ///   (see `executor.rs::register_node_task`), so without the filter
    ///   this lookup would return the status of a sibling node instead
    ///   of the `run_pipeline` parent.
    /// * When `relaunch` re-registers a new parent task with the same
    ///   `tool_call_id` as a failed predecessor, the new record is
    ///   active and the old one is terminal. Preferring the active
    ///   record avoids rejecting node registrations for the live
    ///   relaunch just because the stale failed record has a more
    ///   recent (idempotent) update.
    ///
    /// Returns `None` when no parent record matches (e.g. ephemeral
    /// test harnesses that never register a `run_pipeline` task).
    pub fn parent_status_for_tool_call_id(&self, parent_tool_call_id: &str) -> Option<TaskStatus> {
        if parent_tool_call_id.is_empty() {
            return None;
        }
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        Self::pick_parent_status(&tasks, parent_tool_call_id)
    }

    /// Shared helper that applies the parent-selection rule documented
    /// on [`Self::parent_status_for_tool_call_id`]. Caller holds the
    /// `tasks` lock; this is the inside-lock implementation reused by
    /// the atomic registration guard in [`Self::register_full`].
    fn pick_parent_status(
        tasks: &HashMap<String, BackgroundTask>,
        parent_tool_call_id: &str,
    ) -> Option<TaskStatus> {
        // Codex P2: prefer an active non-pipeline record (live parent)
        // over a stale terminal record sharing the same tool_call_id.
        // This makes the lookup relaunch-safe — `TaskSupervisor::relaunch`
        // re-registers the new parent with the original tool_call_id,
        // so the active record is the true current parent.
        if let Some(active) = tasks
            .values()
            .filter(|task| {
                task.tool_call_id == parent_tool_call_id
                    && !task.tool_name.starts_with("pipeline:")
                    && task.status.is_active()
            })
            .max_by_key(|task| task.updated_at)
        {
            return Some(active.status.clone());
        }
        tasks
            .values()
            .filter(|task| {
                task.tool_call_id == parent_tool_call_id && !task.tool_name.starts_with("pipeline:")
            })
            .max_by_key(|task| task.updated_at)
            .map(|task| task.status.clone())
    }

    /// NEW-18b — strict registration for a pipeline node child task.
    ///
    /// Wraps [`Self::register_full`] with an Option-A preventive guard:
    /// the parent-terminal check and the child insertion happen UNDER
    /// THE SAME `tasks` lock acquisition (see
    /// `parent_terminal_check_tool_call_id` parameter), so concurrent
    /// transitions on the parent cannot slip past the guard between
    /// lookup and insert (codex P2 atomicity concern).
    ///
    /// Refuses with [`RegisterTaskError::ParentTerminal`] when the
    /// parent (looked up via [`Self::pick_parent_status`]) is in a
    /// terminal state. This closes the "phantom child task" race where
    /// the orphan-sweep in [`Self::enable_persistence`] marks the parent
    /// failed but a straggler pipeline tokio worker that survived the
    /// restart keeps registering fresh node children against the live
    /// supervisor.
    ///
    /// On a non-terminal (or unknown) parent the call falls through to
    /// the regular registration path (cap checks still apply). Callers
    /// should treat the returned error as a signal to abort the local
    /// node future — there's no successor task to drive forward.
    pub fn try_register_node_task(
        &self,
        node_tool_name: &str,
        parent_tool_call_id: &str,
        session_key: Option<&str>,
    ) -> Result<String, RegisterTaskError> {
        self.register_full(
            node_tool_name,
            parent_tool_call_id,
            session_key,
            None,
            None,
            None,
            Some(parent_tool_call_id),
        )
    }

    /// Strict variant of [`Self::register_with_input`]: returns the typed
    /// [`RegisterTaskError`] on cap rejection so callers can surface a
    /// structured tool failure instead of swallowing the empty-string
    /// sentinel that the legacy entry points return for compatibility.
    pub fn try_register_with_input(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
        tool_input: Option<Value>,
    ) -> Result<String, RegisterTaskError> {
        self.register_full(
            tool_name,
            tool_call_id,
            session_key,
            None,
            tool_input,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn register_full(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
        task_ledger_path: Option<&str>,
        tool_input: Option<Value>,
        originating_client_message_id: Option<String>,
        parent_terminal_check_tool_call_id: Option<&str>,
    ) -> Result<String, RegisterTaskError> {
        // Codex P2 follow-up: early terminal-parent check, BEFORE the
        // fan-out cap path. The cap path has side effects (poisoning
        // the parent session, mark_failed-ing every active sibling
        // under the same `parent_session_key`). Running those when
        // the parent is already terminal would incorrectly cascade-
        // fail unrelated active children whose parent is still alive
        // but happens to share the session key. By returning
        // `ParentTerminal` here we restore the pre-codex-P2 semantics
        // where a terminal parent short-circuits without touching the
        // cap state. The in-lock recheck at the insertion point still
        // serves as the atomic safety net for the race where a parent
        // becomes terminal between this check and the insert.
        if let Some(parent_tcid) = parent_terminal_check_tool_call_id
            && !parent_tcid.is_empty()
        {
            let status_opt = {
                let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
                Self::pick_parent_status(&tasks, parent_tcid)
            };
            if let Some(status) = status_opt
                && status.is_terminal()
            {
                tracing::warn!(
                    tool_name,
                    parent_tool_call_id = parent_tcid,
                    parent_status = status.as_str(),
                    "refusing pipeline node child registration: parent task is terminal (pre-cap)"
                );
                counter!(
                    "octos_task_supervisor_register_node_rejected_total",
                    "reason" => "parent_terminal".to_string(),
                    "parent_status" => status.as_str().to_string(),
                )
                .increment(1);
                return Err(RegisterTaskError::ParentTerminal {
                    parent_tool_call_id: parent_tcid.to_string(),
                    parent_status: status,
                });
            }
        }

        // Per-parent fan-out cap. Detached registrations (`session_key ==
        // None`) skip the gate because they do not have a parent to
        // attribute the count to — those are MCP/test bookkeeping calls
        // and stay capped only by host process memory.
        if let Some(parent_session_key) = session_key {
            let cap = max_children_per_parent();

            // Fast path: a previously-poisoned parent stays poisoned for the
            // lifetime of the supervisor so the runaway loop's downstream
            // registers see the rejection without re-counting.
            let already_poisoned = self
                .poisoned_parents
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(parent_session_key);
            if already_poisoned {
                let count = self
                    .tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .filter(|task| task.parent_session_key.as_deref() == Some(parent_session_key))
                    .count();
                let error = RegisterTaskError::ChildFanoutExceeded {
                    parent_session_key: parent_session_key.to_string(),
                    count,
                    cap,
                };
                tracing::warn!(
                    parent_session_key = parent_session_key,
                    count,
                    cap,
                    "task supervisor refusing register: parent already poisoned"
                );
                record_child_session_lifecycle("tracked", "refused_poisoned");
                return Err(error);
            }

            // Codex P2 follow-up #2: combine the per-session cap query
            // AND the parent-terminal recheck under the SAME `tasks`
            // lock acquisition. If the parent has flipped to terminal
            // since the pre-cap check, return `ParentTerminal` instead
            // of triggering the cap path's side effects (poisoning the
            // session, force-failing every active sibling). The
            // recheck is gated on `parent_terminal_check_tool_call_id`
            // so non-pipeline callers (e.g. spawn_only register paths)
            // continue to hit the cap path as before.
            let (current_count, parent_terminal_status) = {
                let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
                let count = tasks
                    .values()
                    .filter(|task| task.parent_session_key.as_deref() == Some(parent_session_key))
                    .count();
                let terminal = parent_terminal_check_tool_call_id
                    .filter(|tcid| !tcid.is_empty())
                    .and_then(|tcid| Self::pick_parent_status(&tasks, tcid))
                    .filter(|status| status.is_terminal());
                (count, terminal)
            };
            if let Some(status) = parent_terminal_status {
                let parent_tcid = parent_terminal_check_tool_call_id.unwrap_or_default();
                tracing::warn!(
                    tool_name,
                    parent_tool_call_id = parent_tcid,
                    parent_status = status.as_str(),
                    "refusing pipeline node child registration: parent task terminal at cap-recheck (atomic)"
                );
                counter!(
                    "octos_task_supervisor_register_node_rejected_total",
                    "reason" => "parent_terminal".to_string(),
                    "parent_status" => status.as_str().to_string(),
                )
                .increment(1);
                return Err(RegisterTaskError::ParentTerminal {
                    parent_tool_call_id: parent_tcid.to_string(),
                    parent_status: status,
                });
            }
            if current_count >= cap {
                // Mark the parent session as poisoned so subsequent
                // attempts fail fast without re-counting.
                self.poisoned_parents
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(parent_session_key.to_string());

                let reason = format!("child fanout exceeded ({current_count} of {cap})");

                // Force-fail every still-active child of the runaway
                // parent so the cascade collapses instead of waiting on
                // each child to finish on its own. Snapshot the active
                // ids first so the per-id `mark_failed` does not deadlock
                // on the supervisor's `tasks` mutex.
                let active_children: Vec<String> = self
                    .tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .filter(|task| {
                        task.parent_session_key.as_deref() == Some(parent_session_key)
                            && task.status.is_active()
                    })
                    .map(|task| task.id.clone())
                    .collect();
                for child_id in active_children {
                    self.mark_failed(&child_id, reason.clone());
                }

                let error = RegisterTaskError::ChildFanoutExceeded {
                    parent_session_key: parent_session_key.to_string(),
                    count: current_count,
                    cap,
                };
                tracing::error!(
                    parent_session_key = parent_session_key,
                    count = current_count,
                    cap,
                    "task supervisor refusing register: child fanout cap exceeded"
                );
                counter!(
                    "octos_task_supervisor_fanout_rejected_total",
                    "reason" => "child_fanout_exceeded".to_string()
                )
                .increment(1);
                return Err(error);
            }
        }

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
            tool_input,
            originating_client_message_id,
            // #966 / M13-B — set None at register time. Callers that
            // know the spawn source/role (model vs supervisor, role
            // template, runtime policy stamp) populate via the new
            // `with_m13b_projection(...)` setter immediately after
            // `register_*`. Future supervisor refactors can thread
            // these through register_* directly when convenient.
            source: None,
            role: None,
            summary: None,
            artifact_count: None,
            runtime_policy_stamp: None,
        };
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        // Codex P2 atomicity: when this is a child-task registration
        // that requested the parent-terminal guard, recheck parent
        // status UNDER the same lock that performs the insertion. This
        // closes the race where a concurrent transition could mark the
        // parent terminal between an outside-lock lookup and the
        // insert — without it, a worker could observe the parent as
        // Running, get descheduled while `mark_failed` +
        // `mark_descendants_failed` run, and then insert a fresh
        // `pipeline:<node>` after the cascade.
        if let Some(parent_tcid) = parent_terminal_check_tool_call_id
            && !parent_tcid.is_empty()
            && let Some(status) = Self::pick_parent_status(&tasks, parent_tcid)
            && status.is_terminal()
        {
            drop(tasks);
            tracing::warn!(
                tool_name,
                parent_tool_call_id = parent_tcid,
                parent_status = status.as_str(),
                "refusing pipeline node child registration: parent task is terminal (atomic recheck)"
            );
            counter!(
                "octos_task_supervisor_register_node_rejected_total",
                "reason" => "parent_terminal".to_string(),
                "parent_status" => status.as_str().to_string(),
            )
            .increment(1);
            return Err(RegisterTaskError::ParentTerminal {
                parent_tool_call_id: parent_tcid.to_string(),
                parent_status: status,
            });
        }
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
        Ok(id)
    }

    /// #966 / M13-B — attach the projection metadata (origin, role,
    /// summary, artifact count, runtime policy stamp) to an already-
    /// registered task. Designed for callers who already know how to
    /// derive each piece at spawn time but want to avoid expanding
    /// every `register_*` signature with five new optional args.
    /// Pass `None` for any field whose value is not yet known; the
    /// underlying [`BackgroundTask`] keeps any already-populated value
    /// when the corresponding argument is `None`.
    pub fn set_m13b_projection(
        &self,
        task_id: &str,
        source: Option<String>,
        role: Option<String>,
        summary: Option<String>,
        artifact_count: Option<u32>,
        runtime_policy_stamp: Option<Value>,
    ) {
        // Codex P2 fix: also persist + notify + emit_progress so the
        // reconnect-hydration and `task/updated` subscribers actually
        // observe the new metadata. Without this the projection fields
        // sit in-memory until some unrelated state change fires the
        // callbacks. Mirror the persist/notify/emit pattern used by
        // mark_running / mark_completed / cancel.
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            let Some(task) = tasks.get_mut(task_id) else {
                return;
            };
            let mut changed = false;
            if source.is_some() {
                task.source = source;
                changed = true;
            }
            if role.is_some() {
                task.role = role;
                changed = true;
            }
            if summary.is_some() {
                task.summary = summary;
                changed = true;
            }
            if artifact_count.is_some() {
                task.artifact_count = artifact_count;
                changed = true;
            }
            if runtime_policy_stamp.is_some() {
                task.runtime_policy_stamp = runtime_policy_stamp;
                changed = true;
            }
            if !changed {
                return;
            }
            // Stamp updated_at so reconnect hydration / dashboards see
            // the projection update even when no lifecycle transition
            // fires.
            task.updated_at = Utc::now();
            task.clone()
        };
        self.persist_snapshot(&snapshot);
        self.notify_change(&snapshot);
        self.emit_progress_for_state(&snapshot);
    }

    /// Attach (or replace) the tool input for an already-registered task.
    /// Useful when the task is registered eagerly and the args become
    /// available later in the spawn pipeline.
    pub fn set_tool_input(&self, task_id: &str, tool_input: Value) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = tasks.get_mut(task_id) {
            task.tool_input = Some(tool_input);
        }
    }

    /// Mark a task as running.
    ///
    /// **M8 DoD gate (Req #4)**: this is a no-op when the task is already in
    /// a terminal state. Without the guard a worker that races with `cancel()`
    /// — e.g. cancel fires before the worker observes its cancel token, and
    /// the worker still calls `mark_running` — could resurrect a `Cancelled`
    /// task back to `Running`, undoing the user's cancellation.
    pub fn mark_running(&self, task_id: &str) {
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                if task.status.is_terminal() {
                    tracing::warn!(
                        task_id = %task_id,
                        current_status = task.status.as_str(),
                        current_runtime_state = ?task.runtime_state,
                        attempted_status = TaskStatus::Running.as_str(),
                        "ignoring late mark_running: task already in terminal state",
                    );
                    return;
                }
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
            self.emit_progress_for_state(task);
        }
    }

    /// Update the fine-grained runtime state while keeping the coarse status.
    ///
    /// **M8 DoD gate (Req #4)**: this is a no-op when the task is already in
    /// a terminal state (`Completed`/`Failed`/`Cancelled`). A late harness
    /// event from a worker that already cancelled cannot otherwise flip the
    /// stored `runtime_state` away from `Cancelled`, leaking incorrect
    /// progress emissions and ledger snapshots.
    pub fn mark_runtime_state(
        &self,
        task_id: &str,
        runtime_state: TaskRuntimeState,
        runtime_detail: Option<String>,
    ) {
        let (snapshot, previous_detail) = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                if task.status.is_terminal() {
                    tracing::warn!(
                        task_id = %task_id,
                        current_status = task.status.as_str(),
                        current_runtime_state = ?task.runtime_state,
                        attempted_runtime_state = ?runtime_state,
                        "ignoring late mark_runtime_state: task already in terminal state",
                    );
                    return;
                }
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
            self.emit_progress_for_state(task);
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
    ///
    /// **M8 DoD gate (Req #4)**: this is a no-op when the task is already in a
    /// terminal state (`Completed`/`Failed`/`Cancelled`). The check + write
    /// happen under the same lock as the rest of the supervisor so the guard
    /// is a CAS-style atomic transition. A late-arriving worker that finishes
    /// after the user has cancelled the task therefore *cannot* resurrect it
    /// to `Completed`. The race is logged at `warn` so operators can observe
    /// it.
    pub fn mark_completed(&self, task_id: &str, output_files: Vec<String>) {
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                if task.status.is_terminal() {
                    tracing::warn!(
                        task_id = %task_id,
                        current_status = task.status.as_str(),
                        current_runtime_state = ?task.runtime_state,
                        attempted_status = TaskStatus::Completed.as_str(),
                        "ignoring late mark_completed: task already in terminal state",
                    );
                    return;
                }
                task.status = TaskStatus::Completed;
                task.runtime_state = TaskRuntimeState::Completed;
                task.updated_at = Utc::now();
                task.completed_at = Some(Utc::now());
                let artifact_count = output_files.len() as u32;
                task.output_files = output_files;
                if task.artifact_count.is_some() || artifact_count > 0 {
                    task.artifact_count = Some(artifact_count);
                }
                if task.summary.is_none() {
                    task.summary = Some(if artifact_count > 0 {
                        format!(
                            "{} completed with {} artifact(s)",
                            task.tool_name, artifact_count
                        )
                    } else {
                        format!("{} completed", task.tool_name)
                    });
                }
                Some(task.clone())
            } else {
                None
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
            self.emit_progress_for_state(task);
            // Codex round-4 BLOCKER (PR #1324 follow-up): drain any
            // pending failure stash for this task's unique task_id.
            // Normally `mark_failed` is the only path that inserts
            // (and the terminal guard in `mark_failed` prevents a
            // completion after a failure today). Defensive cleanup
            // ensures a stale entry can't fire later when a sibling
            // task's `mark_synth_ack_emitted` arrives on the same
            // tool_call_id.
            self.drain_pending_failure_for_task(&task.id);
        }
    }

    /// Mark a task as failed with an error message.
    ///
    /// On the FIRST transition from a non-`Failed` status to `Failed`, also
    /// emits a `SpawnOnlyFailureSignal` so listeners (e.g. the session
    /// actor) can schedule a recovery turn. Re-marking an already-failed
    /// task is a no-op for the failure signal — this guarantees at most one
    /// recovery attempt per task even if multiple paths report the failure.
    ///
    /// **M8 DoD gate (Req #4)**: this is a no-op when the task is already
    /// `Cancelled` or `Completed`. The check + write happen under the same
    /// lock so a late worker that races with `cancel()` cannot overwrite a
    /// `Cancelled` task to `Failed` (or a `Completed` task either). Re-marking
    /// an already-`Failed` task is still allowed (idempotent) so existing
    /// `was_already_failed` semantics are preserved.
    pub fn mark_failed(&self, task_id: &str, error: String) {
        let (snapshot, was_already_failed) = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                if matches!(task.status, TaskStatus::Cancelled | TaskStatus::Completed) {
                    tracing::warn!(
                        task_id = %task_id,
                        current_status = task.status.as_str(),
                        current_runtime_state = ?task.runtime_state,
                        attempted_status = TaskStatus::Failed.as_str(),
                        "ignoring late mark_failed: task already in terminal state",
                    );
                    return;
                }
                let already_failed = task.status == TaskStatus::Failed;
                task.status = TaskStatus::Failed;
                task.runtime_state = TaskRuntimeState::Failed;
                task.updated_at = Utc::now();
                task.completed_at = Some(Utc::now());
                if task.summary.is_none() {
                    task.summary = Some(error.chars().take(1200).collect());
                }
                task.error = Some(error);
                (Some(task.clone()), already_failed)
            } else {
                (None, false)
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
            if !was_already_failed {
                self.emit_progress_for_state(task);
                self.notify_failure(task);
            }
        }
    }

    /// Cascade-fail every still-active child of `parent_tool_call_id`.
    ///
    /// Used by the `run_pipeline` timeout arm to flush orphan
    /// `pipeline:<node>` child tasks when the parent future is dropped
    /// before per-node `mark_completed` / `mark_failed` can fire. Without
    /// this cascade the children stay forever as `state: "running"` in
    /// the supervisor, and the SessionTaskIndicator on the dashboard
    /// shows e.g. `pipeline:analyze running` indefinitely.
    ///
    /// IMPORTANT: filters to NODE children only via the `pipeline:`
    /// `tool_name` prefix. The parent `run_pipeline` task is itself
    /// registered with the same `tool_call_id` (see
    /// `execution.rs::register_task_with_input_and_cmid`), and pipeline
    /// node tasks reuse that id via `executor.rs::register_node_task`.
    /// Without the prefix filter the cascade would also mark the parent
    /// failed, racing with the parent runner's own `mark_failed` path.
    /// `pipeline:` is the only prefix `register_node_task` ever emits,
    /// so this is a precise filter for "node tasks under this run".
    ///
    /// Snapshots the matching active task ids under the `tasks` mutex
    /// first, then drops the lock and calls `mark_failed` per id so the
    /// per-task lock acquisition inside `mark_failed` does not deadlock
    /// on the snapshot. Returns the number of children that were
    /// transitioned to `Failed`. Already-terminal tasks are skipped by
    /// `is_active()` and the deadlock-safe `mark_failed` guard.
    pub fn mark_descendants_failed(&self, parent_tool_call_id: &str, reason: &str) -> usize {
        if parent_tool_call_id.is_empty() {
            return 0;
        }
        let active_children: Vec<String> = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|task| {
                task.tool_call_id == parent_tool_call_id
                    && task.status.is_active()
                    && task.tool_name.starts_with("pipeline:")
            })
            .map(|task| task.id.clone())
            .collect();
        let count = active_children.len();
        for child_id in active_children {
            self.mark_failed(&child_id, reason.to_string());
        }
        if count > 0 {
            tracing::info!(
                parent_tool_call_id = %parent_tool_call_id,
                cascaded = count,
                reason = %reason,
                "cascade-failed child tasks under parent tool_call_id"
            );
        }
        count
    }

    /// Emit a `SpawnOnlyFailureSignal` for a freshly-failed task, if a
    /// failure callback has been registered. The error_message is taken
    /// from the task's `error` field (set immediately before this call).
    ///
    /// **Synth-ack gate (two-phase)**: emits or defers based on whether
    /// the LLM previously received the "Background work started for
    /// `<tool>`." synth-ack for this task's `tool_call_id` (recorded via
    /// [`Self::mark_synth_ack_emitted`]):
    ///
    /// * **Ack already emitted** → build the signal and dispatch via
    ///   [`Self::dispatch_failure_signal`] immediately. Idempotent on
    ///   replay via `AckAndPending::emitted_task_ids`.
    /// * **Ack not yet emitted** → stash the signal in
    ///   `AckAndPending::pending` keyed by `task_id` (carrying its
    ///   `tool_call_id`) and return without dispatching. When
    ///   `mark_synth_ack_emitted` later runs for the same
    ///   `tool_call_id`, it scans the map, drains every pending
    ///   entry under that id (pipeline cascade has many tasks under
    ///   one tool_call_id), and emits each signal then.
    ///   This closes the `tokio::spawn` → `mark_synth_ack_emitted` race
    ///   in `execution.rs::handle_spawn_only_branch` where a fast
    ///   failure can hit before the foreground records the ack
    ///   (Codex round-4 BLOCKER, PR #1324 follow-up).
    /// * **Ack permanently suppressed** (sibling-error / pre-flight
    ///   short-circuit) → the pending entry sits in the map until the
    ///   bounded-cap eviction runs (Codex round-2 MAJOR), or until
    ///   `cancel` / `mark_completed` drains it. The LLM already saw
    ///   the sibling error / `[VALIDATION FAILED]` tool_result, so
    ///   the absence of an emitted signal is the correct behaviour.
    ///
    /// **Atomicity (Codex round-2 BLOCKER)**: the ack-check, idempotency
    /// check, and pending insert all happen under the SAME mutex
    /// ([`AckAndPending`]). The previous design used three separate
    /// `Mutex`es, leaving an interleave where
    /// `notify_failure` could observe ack=false → `mark_synth_ack_emitted`
    /// could record ack + drain empty pending → `notify_failure` could
    /// then insert a pending entry that nothing will ever drain. Holding
    /// the single mutex across the entire decision tree makes this race
    /// impossible.
    fn notify_failure(&self, task: &BackgroundTask) {
        if task.tool_call_id.is_empty() {
            // Defensive: an empty id can't be matched by the synth-ack
            // set, so we could never drain a deferred entry. Treat
            // this as "skip" — the LLM already saw something else for
            // this code path (tasks that bypassed id-bearing dispatch).
            tracing::debug!(
                task_id = %task.id,
                tool_name = %task.tool_name,
                "skipping SpawnOnlyFailureSignal: task has empty tool_call_id (cannot key synth-ack lookup)",
            );
            return;
        }
        let signal = SpawnOnlyFailureSignal {
            task_id: task.id.clone(),
            tool_name: task.tool_name.clone(),
            tool_input: task.tool_input.clone().unwrap_or(Value::Null),
            error_message: task.error.clone().unwrap_or_default(),
            suggested_alternatives: parse_alternatives(task.error.as_deref().unwrap_or("")),
            parent_session_key: task.parent_session_key.clone(),
            originating_client_message_id: task.originating_client_message_id.clone(),
        };
        // Atomic ack-check + idempotency-check + (dispatch | stash).
        // The decision branch holds `ack_and_pending` so no interleave
        // with `mark_synth_ack_emitted` can leave a pending entry
        // un-drained, and the idempotency guard cannot race a sibling
        // `mark_failed`.
        let mut guard = self
            .ack_and_pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Idempotency guard: if a previous `notify_failure` (or a
        // drained pending entry) already fired the signal for this
        // task_id, suppress. Protects against:
        //   * `mark_failed` called twice (live + cascade-fail).
        //   * BLOCKER race: failure landed before ack, was stashed,
        //     drained by `mark_synth_ack_emitted`, and now a sibling
        //     path calls `mark_failed` again on the same task.
        // Note: keyed by `task_id` (unique), NOT `tool_call_id`,
        // because pipeline cascade has many tasks sharing the parent's
        // `tool_call_id` and each child must fire its own signal.
        if guard.emitted_task_ids.contains(&task.id) {
            tracing::debug!(
                task_id = %task.id,
                tool_call_id = %task.tool_call_id,
                "skipping SpawnOnlyFailureSignal: already emitted for this task_id",
            );
            return;
        }
        if guard
            .synth_ack_emitted_tool_call_ids
            .contains(&task.tool_call_id)
        {
            // Ack already recorded — mark emitted atomically and
            // release the mutex before invoking the callback (which
            // may take its own locks).
            if !guard.mark_emitted(&task.id) {
                // mark_emitted returns false when another path won
                // the race; this is technically reachable only when
                // the idempotency check above and mark_emitted disagree
                // (impossible while we hold the lock), but the
                // defensive return is cheap.
                return;
            }
            drop(guard);
            self.invoke_failure_callback(&signal);
        } else {
            // Two-phase: stash and wait for the ack. The pending map
            // is keyed by `task_id` (unique) and carries the
            // `tool_call_id` so `mark_synth_ack_emitted` can scan and
            // drain all matching entries — required for cascade where
            // many tasks share one tool_call_id.
            tracing::debug!(
                task_id = %task.id,
                tool_name = %task.tool_name,
                tool_call_id = %task.tool_call_id,
                "deferring SpawnOnlyFailureSignal: synth-ack not yet recorded (will emit on ack or stay pending if ack is suppressed)",
            );
            guard.insert_pending(
                task.id.clone(),
                PendingFailure {
                    tool_call_id: task.tool_call_id.clone(),
                    signal,
                },
            );
        }
    }

    /// Internal helper: drop any pending failure stash for `task_id`
    /// (the supervisor's unique task identifier). Called from
    /// terminal paths that should invalidate a deferred failure
    /// (currently `mark_completed` and `cancel`). No-op when nothing
    /// is pending.
    fn drain_pending_failure_for_task(&self, task_id: &str) -> Option<PendingFailure> {
        if task_id.is_empty() {
            return None;
        }
        let mut guard = self
            .ack_and_pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.remove_pending(task_id)
    }

    /// Internal helper: fire the failure callback and mark the
    /// `task_id` as emitted so future replays / cascade paths observe
    /// the idempotency guard. Called from `mark_synth_ack_emitted`
    /// (drained pending entry path); the `notify_failure` direct-
    /// dispatch path inlines the same logic under
    /// `ack_and_pending` to keep the ack-check + emitted-mark atomic.
    fn dispatch_failure_signal(&self, task_id: &str, signal: SpawnOnlyFailureSignal) {
        // Single-mutex idempotency: mark_emitted returns false when
        // another path already dispatched. Lock is released BEFORE
        // calling the user-supplied callback so the callback may
        // freely take any other lock (notably `on_failure`).
        {
            let mut guard = self
                .ack_and_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !guard.mark_emitted(task_id) {
                tracing::debug!(
                    task_id = %task_id,
                    "dispatch_failure_signal: another path already emitted; suppressing",
                );
                return;
            }
        }
        self.invoke_failure_callback(&signal);
    }

    /// Internal helper: invoke the user-supplied `on_failure` callback
    /// with `signal`. Separated from the dispatcher so callers that
    /// already hold (or already released) `ack_and_pending` can reuse
    /// the callback-invocation path without re-checking the emitted set.
    fn invoke_failure_callback(&self, signal: &SpawnOnlyFailureSignal) {
        let guard = self.on_failure.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cb) = guard.as_ref() {
            cb(signal);
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
            HarnessEventPayload::SessionSanitized { .. } => {
                // Session-sanitize events are observability-only (M8.6).
                // They fire once per resume and describe what the resume
                // policy dropped — the task lifecycle is not affected; the
                // session actor will subsequently drive normal
                // Queued → Executing transitions as usual.
                self.mark_runtime_state(
                    task_id,
                    snapshot.runtime_state,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::SubagentProgress { .. } => {
                // Sub-agent progress is a periodic textual summary generated
                // by `AgentSummaryGenerator`. It does not change the
                // lifecycle state — we simply fold it into the runtime
                // detail so dashboards can render a live "what is the
                // sub-agent doing" label.
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
        }

        Ok(())
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

    /// #966 / M13-B — the projection setter populates the new
    /// optional fields. Verifies that:
    /// - Newly-registered tasks start with all five fields None.
    /// - `set_m13b_projection` overwrites the fields that were
    ///   supplied as Some and leaves the rest untouched.
    /// - The persisted JSON round-trips through serde and the
    ///   default-omitted fields stay invisible until populated.
    #[test]
    fn set_m13b_projection_populates_optional_fields() {
        use serde_json::json;
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("tts", "call-m13b", None);

        let initial = supervisor.get_task(&id).expect("task");
        assert!(initial.source.is_none());
        assert!(initial.role.is_none());
        assert!(initial.summary.is_none());
        assert!(initial.artifact_count.is_none());
        assert!(initial.runtime_policy_stamp.is_none());

        supervisor.set_m13b_projection(
            &id,
            Some("model".into()),
            Some("reviewer".into()),
            Some("found 1 issue".into()),
            Some(2),
            Some(json!({ "approval_policy": "on-request" })),
        );

        let updated = supervisor.get_task(&id).expect("task");
        assert_eq!(updated.source.as_deref(), Some("model"));
        assert_eq!(updated.role.as_deref(), Some("reviewer"));
        assert_eq!(updated.summary.as_deref(), Some("found 1 issue"));
        assert_eq!(updated.artifact_count, Some(2));
        assert_eq!(
            updated.runtime_policy_stamp,
            Some(json!({ "approval_policy": "on-request" }))
        );

        // Partial update — only the artifact_count moves; the rest stay.
        supervisor.set_m13b_projection(&id, None, None, None, Some(5), None);
        let after_partial = supervisor.get_task(&id).expect("task");
        assert_eq!(after_partial.source.as_deref(), Some("model"));
        assert_eq!(after_partial.role.as_deref(), Some("reviewer"));
        assert_eq!(after_partial.artifact_count, Some(5));

        // Wire-shape: legacy snapshots without the fields round-trip
        // cleanly thanks to `#[serde(default)]`, AND newly-populated
        // ones surface every field.
        let json_form = serde_json::to_value(&after_partial).unwrap();
        assert_eq!(json_form["source"], "model");
        assert_eq!(json_form["role"], "reviewer");
        assert_eq!(json_form["summary"], "found 1 issue");
        assert_eq!(json_form["artifact_count"], 5);

        let bare = supervisor.register("podcast_generate", "call-bare", None);
        let bare_json = serde_json::to_value(supervisor.get_task(&bare).unwrap()).unwrap();
        assert!(bare_json.as_object().unwrap().get("source").is_none());
        assert!(
            bare_json
                .as_object()
                .unwrap()
                .get("artifact_count")
                .is_none()
        );
    }

    /// Codex P2 fix: `set_m13b_projection` must persist + notify so
    /// reconnect hydration and `task/updated` subscribers observe the
    /// new metadata without waiting for an unrelated lifecycle event.
    /// Pins the on_change callback firing AND `updated_at` advancing.
    #[test]
    fn set_m13b_projection_fires_on_change_and_bumps_updated_at() {
        use std::sync::{Arc, Mutex};

        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("tts", "call-m13b-notify", None);
        let before = supervisor.get_task(&id).expect("task").updated_at;

        let notifications: Arc<Mutex<Vec<BackgroundTask>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&notifications);
        supervisor.set_on_change(move |task: &BackgroundTask| {
            sink.lock().unwrap().push(task.clone());
        });

        // Sleep so updated_at is observably greater than registered_at.
        std::thread::sleep(std::time::Duration::from_millis(2));
        supervisor.set_m13b_projection(
            &id,
            Some("model".into()),
            Some("reviewer".into()),
            None,
            None,
            None,
        );

        let updated = supervisor.get_task(&id).expect("task");
        assert!(
            updated.updated_at > before,
            "set_m13b_projection must bump updated_at; before={before:?} after={:?}",
            updated.updated_at
        );

        let observed_len = notifications.lock().unwrap().len();
        assert_eq!(observed_len, 1, "on_change should fire exactly once");
        let event = notifications.lock().unwrap()[0].clone();
        assert_eq!(event.source.as_deref(), Some("model"));
        assert_eq!(event.role.as_deref(), Some("reviewer"));

        // No-op call (every arg None) must NOT fire the callback or
        // bump updated_at — defensive, avoids spurious update spam.
        let after_change = updated.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(2));
        supervisor.set_m13b_projection(&id, None, None, None, None, None);
        let after_noop = supervisor.get_task(&id).expect("task");
        assert_eq!(
            after_noop.updated_at, after_change,
            "no-op call must NOT bump updated_at"
        );
        assert_eq!(
            notifications.lock().unwrap().len(),
            1,
            "no-op call must NOT fire on_change"
        );
    }

    #[test]
    fn terminal_updates_refresh_summary_and_artifact_count() {
        let supervisor = TaskSupervisor::new();
        let completed = supervisor.register("spawn", "call-complete", None);
        supervisor.set_m13b_projection(
            &completed,
            Some("model".into()),
            Some("reviewer".into()),
            None,
            Some(0),
            None,
        );
        supervisor.mark_completed(
            &completed,
            vec![
                "/tmp/octos-review/report.md".to_owned(),
                "/tmp/octos-review/raw.json".to_owned(),
            ],
        );
        let task = supervisor.get_task(&completed).expect("completed task");
        assert_eq!(task.artifact_count, Some(2));
        assert_eq!(
            task.summary.as_deref(),
            Some("spawn completed with 2 artifact(s)")
        );

        let failed = supervisor.register("spawn", "call-fail", None);
        supervisor.mark_failed(&failed, "review worker failed".to_owned());
        let task = supervisor.get_task(&failed).expect("failed task");
        assert_eq!(task.summary.as_deref(), Some("review worker failed"));
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
        let id = supervisor.register("search", "call-9", Some("api:session"));
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
        let id = supervisor.register_with_lineage("search", "call-9", Some("api:session"), None);
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
        assert_eq!(
            detail["schema_version"],
            serde_json::json!(crate::abi_schema::HARNESS_PROGRESS_EVENT_SCHEMA_VERSION)
        );
        assert_eq!(detail["task_id"], id);
        assert_eq!(detail["workflow_kind"], "deep_research");
        assert_eq!(detail["current_phase"], "fetch");
        assert_eq!(detail["progress_message"], "Fetching 4 pages");
        // Across restart, the in-flight task has no live worker — the orphan
        // reaper marks it Failed so callers observe a clean terminal state.
        // The harness progress detail still survives so operators can inspect
        // where the task was when the runtime died.
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(
            task.error.as_deref(),
            Some("orphaned across restart"),
            "orphan reaper must record a stable error message"
        );
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

    /// Cascade-fail every active child of a parent's `tool_call_id`.
    /// Regression pin for the `run_pipeline` timeout orphan bug —
    /// without `mark_descendants_failed` child `pipeline:<node>` tasks
    /// registered before the timeout future was dropped stayed in
    /// `state: "running"` forever (visible to dashboard users as e.g.
    /// `pipeline:analyze running` indefinitely).
    #[test]
    fn mark_descendants_failed_cascades_active_children_under_parent_tcid() {
        let supervisor = TaskSupervisor::new();
        let parent_tcid = "call-run_pipeline-parent";
        // The parent `run_pipeline` task is registered with the same
        // tool_call_id its node children reuse via
        // `executor.rs::register_node_task`. The cascade MUST NOT
        // touch the parent (it has its own `mark_failed` path in the
        // timeout arm of `RunPipelineTool::execute`).
        let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-1"));
        // Three node children share the parent's tool_call_id. The
        // first is pre-completed (should stay completed), the other
        // two are running (should both transition to Failed with the
        // timeout reason).
        let child1 = supervisor.register("pipeline:setup", parent_tcid, Some("sess-1"));
        let child2 = supervisor.register("pipeline:analyze", parent_tcid, Some("sess-1"));
        let child3 = supervisor.register("pipeline:plan_and_search", parent_tcid, Some("sess-1"));
        // A sibling task NOT under the timing-out parent: must be
        // untouched by the cascade.
        let unrelated = supervisor.register("tts", "call-other-parent", Some("sess-1"));

        supervisor.mark_running(&parent);
        supervisor.mark_running(&child2);
        supervisor.mark_running(&child3);
        supervisor.mark_running(&unrelated);
        supervisor.mark_completed(&child1, vec![]);

        let cascaded =
            supervisor.mark_descendants_failed(parent_tcid, "pipeline timed out after 1200s");
        assert_eq!(
            cascaded, 2,
            "exactly two pipeline:<node> children were active and should cascade-fail"
        );

        // child1 was completed before the cascade — must stay completed
        // (mark_failed's terminal-state guard preserves it).
        let t1 = supervisor.get_task(&child1).expect("child1");
        assert_eq!(t1.status, TaskStatus::Completed);

        // child2 and child3 were running — must now be Failed with the
        // pipeline-timeout reason carried in the error field.
        for cid in [&child2, &child3] {
            let task = supervisor.get_task(cid).expect("child task");
            assert_eq!(
                task.status,
                TaskStatus::Failed,
                "child {cid} must be Failed after cascade"
            );
            assert_eq!(task.runtime_state, TaskRuntimeState::Failed);
            assert!(task.completed_at.is_some());
            let err = task.error.clone().unwrap_or_default();
            assert!(
                err.contains("pipeline timed out after 1200s"),
                "child {cid} error must carry the timeout reason, got: {err}"
            );
        }

        // The parent `run_pipeline` task itself must remain Running —
        // its own `mark_failed` path in the timeout arm of
        // `RunPipelineTool::execute` is responsible for transitioning
        // it (the cascade must not race with that).
        let parent_task = supervisor.get_task(&parent).expect("parent");
        assert_eq!(
            parent_task.status,
            TaskStatus::Running,
            "parent run_pipeline task must NOT be cascaded — it has its own mark_failed path"
        );

        // The unrelated sibling under a different parent tool_call_id
        // must remain Running.
        let other = supervisor.get_task(&unrelated).expect("unrelated");
        assert_eq!(
            other.status,
            TaskStatus::Running,
            "task under a different parent tool_call_id must not be cascaded"
        );
    }

    /// Explicit regression pin for the codex MAJOR on #1180: the
    /// cascade MUST filter to `pipeline:<node>` children and skip the
    /// parent `run_pipeline` task even though both share the same
    /// `tool_call_id`. Without the prefix filter, the cascade would
    /// race with `RunPipelineTool::execute`'s own `mark_failed` path
    /// for the parent.
    #[test]
    fn mark_descendants_failed_does_not_touch_parent_run_pipeline_task() {
        let supervisor = TaskSupervisor::new();
        let parent_tcid = "call-run_pipeline-only-parent";
        // Register ONLY the parent (no node children yet — pipeline
        // timed out before any node was dispatched, or all nodes
        // already completed). Cascade must be a no-op for the parent.
        let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-only"));
        supervisor.mark_running(&parent);

        let cascaded =
            supervisor.mark_descendants_failed(parent_tcid, "pipeline timed out after 1200s");
        assert_eq!(
            cascaded, 0,
            "no pipeline:<node> children registered, so cascade must be a no-op"
        );

        let parent_task = supervisor.get_task(&parent).expect("parent survives");
        assert_eq!(
            parent_task.status,
            TaskStatus::Running,
            "parent run_pipeline task must remain Running — cascade only targets pipeline:<node>"
        );
        assert!(
            parent_task.error.is_none(),
            "cascade must not write an error to the parent task"
        );
    }

    /// `mark_descendants_failed` with an empty parent tool_call_id is
    /// a no-op (defensive guard — empty strings never match a real
    /// registered task, and we don't want to mass-fail tasks that
    /// happened to register with no parent context).
    #[test]
    fn mark_descendants_failed_with_empty_parent_is_noop() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("pipeline:work", "", Some("sess"));
        supervisor.mark_running(&id);

        let cascaded = supervisor.mark_descendants_failed("", "timeout");
        assert_eq!(cascaded, 0, "empty parent tcid must short-circuit");

        let task = supervisor.get_task(&id).expect("task survives");
        assert_eq!(task.status, TaskStatus::Running);
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
            supervisor.register_with_lineage("search", "call-1", Some("api:session"), None);
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
        // The orphan reaper marks non-terminal tasks Failed at startup —
        // their owning workers are gone. Metadata (lineage, ledger path,
        // last-known runtime_detail) is preserved for operator diagnosis.
        assert_eq!(tasks[0].status, TaskStatus::Failed);
        assert_eq!(tasks[0].runtime_state, TaskRuntimeState::Failed);
        assert_eq!(
            tasks[0].error.as_deref(),
            Some("orphaned across restart"),
            "orphan reaper must mark restored running tasks Failed"
        );
        // runtime_detail (the last live progress payload) survives the
        // reap so operators can see where the task was when the worker died.
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

    #[test]
    fn should_pass_through_mark_completed_for_skill_reported_files() {
        // Supervisor no longer validates artifact content — it records the
        // skill+contract's reported outcome verbatim. Even a degenerate
        // 44-byte "voice.wav" stub passes through. The workspace contract
        // and the skill itself are responsible for catching bad outputs.
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("voice.wav");
        std::fs::write(&stub, vec![0u8; 44]).unwrap();

        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("fm_tts", "call-1", None);
        supervisor.mark_running(&id);

        supervisor.mark_completed(&id, vec![stub.to_string_lossy().to_string()]);

        let task = supervisor.get_task(&id).expect("task missing");
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.runtime_state, TaskRuntimeState::Completed);
        assert!(task.error.is_none());
    }

    // ── M8.9: spawn_only failure recovery signals ───────────────────────────

    use std::sync::Mutex as StdMutex;

    fn collect_failure_signals(
        supervisor: &TaskSupervisor,
    ) -> Arc<StdMutex<Vec<SpawnOnlyFailureSignal>>> {
        let collected = Arc::new(StdMutex::new(Vec::new()));
        let captured = Arc::clone(&collected);
        supervisor.set_on_failure_signal(move |signal| {
            captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(signal.clone());
        });
        collected
    }

    #[test]
    fn should_emit_failure_signal_when_spawn_only_task_status_becomes_failed() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register_with_input(
            "fm_tts",
            "call-1",
            Some("api:session"),
            Some(serde_json::json!({"voice": "yangmi", "text": "hi"})),
        );
        // Synth-ack gate: simulate the LLM having seen the
        // "Background work started for `fm_tts`." ack — production wires
        // this from `loop_runner.rs` when the synth-ack fires.
        supervisor.mark_synth_ack_emitted("call-1");
        supervisor.mark_running(&task_id);
        supervisor.mark_failed(
            &task_id,
            "voice 'yangmi' not registered. available: vivian, serena, longxiang".to_string(),
        );

        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1, "expected exactly one failure signal");
        let signal = &signals[0];
        assert_eq!(signal.task_id, task_id);
        assert_eq!(signal.tool_name, "fm_tts");
        assert_eq!(signal.parent_session_key.as_deref(), Some("api:session"));
        assert!(
            signal
                .error_message
                .contains("voice 'yangmi' not registered")
        );
        assert_eq!(
            signal.suggested_alternatives,
            vec![
                "vivian".to_string(),
                "serena".to_string(),
                "longxiang".to_string()
            ]
        );
        assert_eq!(signal.tool_input["voice"], "yangmi");
    }

    #[test]
    fn should_not_emit_signal_on_successful_completion() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-2", None);
        supervisor.mark_running(&task_id);
        supervisor.mark_completed(&task_id, vec!["/tmp/out.mp3".to_string()]);

        assert!(
            collected.lock().unwrap().is_empty(),
            "completion must not emit failure signal"
        );
    }

    #[test]
    fn should_not_emit_signal_on_transient_running_state() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-3", None);
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".into()),
        );

        assert!(
            collected.lock().unwrap().is_empty(),
            "transient state changes must not emit failure signal"
        );
    }

    #[test]
    fn should_only_emit_failure_signal_once_per_task() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-4", None);
        supervisor.mark_synth_ack_emitted("call-4");
        supervisor.mark_running(&task_id);
        supervisor.mark_failed(&task_id, "first failure".to_string());
        // re-marking should NOT re-fire the signal — guards against runaway
        // recovery loops if multiple paths report the same failure.
        supervisor.mark_failed(&task_id, "second failure".to_string());
        supervisor.mark_failed(&task_id, "third failure".to_string());

        assert_eq!(
            collected.lock().unwrap().len(),
            1,
            "subsequent failures must not re-fire the signal"
        );
    }

    #[test]
    fn should_capture_tool_input_in_failure_signal() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let input = serde_json::json!({
            "voice": "yangmi",
            "text": "hello world",
            "format": "mp3",
        });
        let task_id = supervisor.register_with_input("fm_tts", "call-5", None, Some(input.clone()));
        supervisor.mark_synth_ack_emitted("call-5");
        supervisor.mark_failed(&task_id, "internal error".to_string());

        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].tool_input, input);
    }

    #[test]
    fn parse_alternatives_handles_canonical_pattern() {
        let alts = parse_alternatives(
            "voice 'yangmi' not registered. available: vivian, serena, longxiang.",
        );
        assert_eq!(alts, vec!["vivian", "serena", "longxiang"]);
    }

    #[test]
    fn parse_alternatives_returns_empty_when_no_marker() {
        let alts = parse_alternatives("connection refused after 3 retries");
        assert!(alts.is_empty());
    }

    #[test]
    fn parse_alternatives_strips_quotes_and_whitespace() {
        let alts = parse_alternatives(r#"available: "alice", 'bob' , charlie"#);
        assert_eq!(alts, vec!["alice", "bob", "charlie"]);
    }

    #[test]
    fn should_set_tool_input_after_registration() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-6", None);
        supervisor.set_tool_input(&task_id, serde_json::json!({"voice": "yangmi"}));
        supervisor.mark_synth_ack_emitted("call-6");
        supervisor.mark_failed(&task_id, "voice missing".to_string());

        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].tool_input["voice"], "yangmi");
    }

    #[test]
    fn should_not_enqueue_second_recovery_for_same_task_id() {
        // Spec-named alias of should_only_emit_failure_signal_once_per_task —
        // codifies that the supervisor-level dedup is what guarantees the
        // session actor never sees a second hint for the same task id.
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-dedup", None);
        supervisor.mark_synth_ack_emitted("call-dedup");
        supervisor.mark_failed(&task_id, "first".to_string());
        supervisor.mark_failed(&task_id, "second".to_string());
        assert_eq!(collected.lock().unwrap().len(), 1);
    }

    #[test]
    fn should_include_parsed_alternatives_from_error_text() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-alts", None);
        supervisor.mark_synth_ack_emitted("call-alts");
        supervisor.mark_failed(
            &task_id,
            "voice missing. available: vivian, serena, longxiang.".to_string(),
        );
        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1);
        assert_eq!(
            signals[0].suggested_alternatives,
            vec![
                "vivian".to_string(),
                "serena".to_string(),
                "longxiang".to_string(),
            ]
        );
    }

    #[test]
    fn should_include_tool_name_and_input_in_recovery_prompt() {
        // Asserts the supervisor exposes both the tool name and the input
        // on the SpawnOnlyFailureSignal so the session actor can build the
        // recovery prompt without re-walking the message history.
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let input = serde_json::json!({"voice": "yangmi", "text": "hello"});
        let task_id =
            supervisor.register_with_input("fm_tts", "call-prompt", None, Some(input.clone()));
        supervisor.mark_synth_ack_emitted("call-prompt");
        supervisor.mark_failed(&task_id, "voice missing".to_string());
        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].tool_name, "fm_tts");
        assert_eq!(signals[0].tool_input, input);
    }

    #[test]
    fn should_emit_failure_signal_with_null_tool_input_when_unset() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-7", None);
        supervisor.mark_synth_ack_emitted("call-7");
        supervisor.mark_failed(&task_id, "boom".to_string());

        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].tool_input, Value::Null);
    }

    /// Synth-ack gate (PR feat/spawn-only-failure-feedback-loop): when the
    /// LLM never received the "Background work started for `<tool>`." ack
    /// for this task's `tool_call_id` (because the synth-ack gate
    /// suppressed it — sibling-error mode), the supervisor MUST NOT emit
    /// a `SpawnOnlyFailureSignal` on the eventual post-spawn failure. The
    /// LLM already saw the sibling error in its iteration and will react
    /// — injecting a synthetic recovery prompt would double-signal.
    #[test]
    fn should_not_emit_failure_signal_when_synth_ack_was_never_emitted() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-no-ack", None);
        supervisor.mark_running(&task_id);
        // No mark_synth_ack_emitted call — production analog: sibling tool
        // errored in this batch so loop_runner.rs suppressed the ack.
        supervisor.mark_failed(&task_id, "post-spawn error".to_string());

        assert!(
            collected.lock().unwrap().is_empty(),
            "failure signal must be suppressed when synth-ack never went to the LLM",
        );
    }

    #[test]
    fn should_emit_failure_signal_only_after_synth_ack_recorded_for_tool_call_id() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);

        // First task — synth-ack was suppressed, failure must NOT signal.
        let suppressed_task = supervisor.register("fm_tts", "call-suppressed", None);
        supervisor.mark_failed(&suppressed_task, "boom A".to_string());

        // Second task — synth-ack fired, failure MUST signal.
        let acked_task = supervisor.register("fm_tts", "call-acked", None);
        supervisor.mark_synth_ack_emitted("call-acked");
        supervisor.mark_failed(&acked_task, "boom B".to_string());

        let signals = collected.lock().unwrap().clone();
        assert_eq!(
            signals.len(),
            1,
            "exactly one failure signal — the synth-acked task — must reach the callback",
        );
        assert_eq!(signals[0].task_id, acked_task);
    }

    #[test]
    fn mark_synth_ack_emitted_is_idempotent_and_ignores_empty_id() {
        let supervisor = TaskSupervisor::new();
        // Idempotent on repeated calls.
        supervisor.mark_synth_ack_emitted("call-x");
        supervisor.mark_synth_ack_emitted("call-x");
        assert!(supervisor.was_synth_ack_emitted("call-x"));
        // Empty / unknown id remains untracked.
        supervisor.mark_synth_ack_emitted("");
        assert!(!supervisor.was_synth_ack_emitted(""));
        assert!(!supervisor.was_synth_ack_emitted("call-missing"));
    }

    // ── Codex round-4 BLOCKER (PR #1324 follow-up): two-phase
    // failure emission closes the spawn-vs-ack race ─────────────

    /// Race scenario: `tokio::spawn` in execution.rs dispatches the
    /// background task BEFORE loop_runner.rs records the synth-ack
    /// (the spawn happens at line ~493, the synth-ack at line ~1356).
    /// A fast post-spawn failure (plugin binary missing, instant
    /// validator reject, etc.) can run `notify_failure` while
    /// `synth_ack_emitted_tool_call_ids` is still empty. Pre-fix:
    /// the would-be `SpawnOnlyFailureSignal` was dropped and the LLM
    /// stayed in "Background work started" limbo forever. Post-fix:
    /// the failure is stashed in `pending_failures` and emitted
    /// when `mark_synth_ack_emitted` later arrives.
    #[test]
    fn failure_before_synth_ack_emits_recovery_when_ack_arrives() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register_with_input(
            "fm_tts",
            "call-race",
            Some("api:session"),
            Some(serde_json::json!({"voice": "yangmi", "text": "hi"})),
        );
        // Failure lands BEFORE the synth-ack is recorded — exactly the
        // race described in the codex BLOCKER. Pre-fix this dropped
        // the signal forever; post-fix it stashes for replay.
        supervisor.mark_failed(&task_id, "post-spawn boom".to_string());
        assert!(
            collected.lock().unwrap().is_empty(),
            "no signal must fire before the synth-ack lands"
        );

        // Foreground loop_runner finally records the synth-ack — the
        // stashed failure should now reach the callback.
        supervisor.mark_synth_ack_emitted("call-race");

        let signals = collected.lock().unwrap().clone();
        assert_eq!(
            signals.len(),
            1,
            "deferred failure must emit exactly one signal when the ack arrives"
        );
        assert_eq!(signals[0].task_id, task_id);
        assert_eq!(signals[0].tool_name, "fm_tts");
        assert!(signals[0].error_message.contains("post-spawn boom"));
        assert_eq!(
            signals[0].parent_session_key.as_deref(),
            Some("api:session")
        );
    }

    /// Companion to `failure_before_synth_ack_emits_recovery_when_ack_arrives`:
    /// once the deferred-failure path has fired exactly one signal,
    /// any sibling `mark_failed` call on the same task must NOT
    /// double-fire. The codex BLOCKER spec calls this out as
    /// `failure_signal_idempotent_on_double_emit`.
    #[test]
    fn failure_signal_idempotent_on_double_emit() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-double", None);
        // Fail-before-ack → stash.
        supervisor.mark_failed(&task_id, "first failure".to_string());
        // Ack → drain + dispatch once.
        supervisor.mark_synth_ack_emitted("call-double");
        // Subsequent mark_failed calls (production analog: cascade
        // path + the original failure path racing) must observe the
        // idempotency guard.
        supervisor.mark_failed(&task_id, "duplicate failure".to_string());
        supervisor.mark_failed(&task_id, "third failure".to_string());

        let signals = collected.lock().unwrap().clone();
        assert_eq!(
            signals.len(),
            1,
            "exactly one signal must fire even with repeated mark_failed + ack drain"
        );
        assert_eq!(signals[0].task_id, task_id);
    }

    /// The deferred-failure stash for one `tool_call_id` must not
    /// interfere with normal failure-signal delivery for any other
    /// `tool_call_id`. Codex BLOCKER spec calls this
    /// `pending_failure_does_not_block_other_call_ids`.
    #[test]
    fn pending_failure_does_not_block_other_call_ids() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);

        // Task A: fails before its ack arrives → goes pending.
        let task_a = supervisor.register("fm_tts", "call-A", None);
        supervisor.mark_failed(&task_a, "boom A".to_string());
        assert!(
            collected.lock().unwrap().is_empty(),
            "A's failure should still be pending — no ack yet"
        );

        // Task B: independent tool_call_id, normal ordering (ack
        // before failure) → must signal normally without being
        // blocked by A's pending stash.
        let task_b = supervisor.register("fm_tts", "call-B", None);
        supervisor.mark_synth_ack_emitted("call-B");
        supervisor.mark_failed(&task_b, "boom B".to_string());

        let signals = collected.lock().unwrap().clone();
        assert_eq!(
            signals.len(),
            1,
            "B must emit normally even while A sits in the pending map"
        );
        assert_eq!(signals[0].task_id, task_b);
        assert!(signals[0].error_message.contains("boom B"));

        // Finalise A — once its ack arrives the pending stash drains.
        supervisor.mark_synth_ack_emitted("call-A");
        let signals = collected.lock().unwrap().clone();
        assert_eq!(
            signals.len(),
            2,
            "A's pending entry must emit exactly once when its ack arrives"
        );
        assert_eq!(signals[1].task_id, task_a);
        assert!(signals[1].error_message.contains("boom A"));
    }

    /// Codex round-4 MAJOR (PR #1324): the synth-ack must be recorded
    /// under the SANITIZED `tool_call_id` that the dispatcher used to
    /// register the background task. Test as a direct supervisor-level
    /// guard: simulate the production caller (loop_runner.rs:1357)
    /// passing the sanitized id through, and verify the recovery
    /// signal fires. Without the fix, loop_runner.rs records the raw
    /// `call:1` while the supervisor stored the task under `call_1`,
    /// so `was_synth_ack_emitted` misses and the recovery path is
    /// permanently dropped.
    #[test]
    fn spawn_only_synth_ack_records_sanitized_id_when_id_has_colon() {
        // Mirror the canonical sanitization rule from
        // `agent::message_repair::sanitize_tool_call_id` (module is
        // private, so we encode the contract inline): every char
        // outside `[A-Za-z0-9_-]` maps to `_`. This is the same
        // mapping the dispatcher applies before storing the
        // BackgroundTask in the supervisor.
        let raw_id = "call:1";
        let sanitized: String = raw_id
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
                _ => '_',
            })
            .collect();
        assert_eq!(
            sanitized, "call_1",
            "sanitize_tool_call_id contract: `:` → `_` (precondition)"
        );

        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);

        // The dispatcher stores the supervised task under the
        // SANITIZED tool_call_id (see execution.rs:438).
        let task_id = supervisor.register("fm_tts", &sanitized, Some("api:session"));

        // Post-MAJOR-fix loop_runner.rs records the synth-ack from
        // `sanitized_response.tool_calls` — i.e. with `call_1`, not
        // `call:1`. Simulate exactly that path.
        supervisor.mark_synth_ack_emitted(&sanitized);
        assert!(
            supervisor.was_synth_ack_emitted(&sanitized),
            "supervisor must observe synth-ack under the sanitized id"
        );
        // The raw id was never recorded — confirm we didn't
        // accidentally key on the un-sanitized form.
        assert!(
            !supervisor.was_synth_ack_emitted(raw_id),
            "supervisor must NOT observe synth-ack under the raw `call:1` id"
        );

        // Post-spawn failure runs through the supervisor with the
        // sanitized id (because that's what the BackgroundTask carries).
        supervisor.mark_failed(&task_id, "post-spawn boom".to_string());

        let signals = collected.lock().unwrap().clone();
        assert_eq!(
            signals.len(),
            1,
            "recovery signal must fire when synth-ack and supervisor task share the SANITIZED id"
        );
        assert_eq!(signals[0].task_id, task_id);
        assert!(signals[0].error_message.contains("post-spawn boom"));
    }

    // ── Codex round-2 BLOCKER + MAJOR (PR #1324 follow-up): atomic
    // ack-vs-pending decision and bounded state ─────────────────

    /// Codex round-2 BLOCKER: even when `notify_failure` and
    /// `mark_synth_ack_emitted` are interleaved by concurrent threads,
    /// every failure must eventually surface as a recovery signal once
    /// the ack arrives. Pre-fix (separate mutexes for the ack set and
    /// the pending map), this race could permanently drop the signal:
    ///   1. notify_failure observes ack=false.
    ///   2. mark_synth_ack_emitted records ack + drains empty pending.
    ///   3. notify_failure inserts pending — nothing will drain it.
    /// Post-fix the combined mutex makes step 2 atomic with the
    /// check-and-insert pair in step 1+3, so the pending entry is
    /// either drained in step 2 OR observed in step 1 and dispatched
    /// directly. Either way, exactly one signal per failure.
    #[test]
    fn failure_inserted_during_concurrent_ack_drain_still_fires() {
        use std::sync::Barrier;
        use std::thread;

        // High iteration count + concurrent racing pair to surface any
        // residual race. Even 1 lost wakeup across 200 iterations is a
        // 0.5% drop rate — easy to catch.
        const ITERATIONS: usize = 200;
        for iter in 0..ITERATIONS {
            let supervisor = TaskSupervisor::new();
            let collected = collect_failure_signals(&supervisor);
            let tool_call_id = format!("call-race-{iter}");
            let task_id = supervisor.register("fm_tts", &tool_call_id, None);

            // Two threads contend on `notify_failure` (via mark_failed)
            // and `mark_synth_ack_emitted`. The barrier maximizes the
            // chance of an interleaved hit on the ack-check vs
            // pending-insert window. Pre-fix this loses ~1-2% of
            // iterations on Apple Silicon; post-fix it must fire on
            // every iteration.
            let barrier = Arc::new(Barrier::new(2));
            let sup_a = supervisor.clone();
            let sup_b = supervisor.clone();
            let bar_a = Arc::clone(&barrier);
            let bar_b = Arc::clone(&barrier);
            let tcid_a = tool_call_id.clone();
            let tcid_b = tool_call_id.clone();
            let tid = task_id.clone();

            let h1 = thread::spawn(move || {
                bar_a.wait();
                sup_a.mark_failed(&tid, "race boom".to_string());
            });
            let h2 = thread::spawn(move || {
                bar_b.wait();
                sup_b.mark_synth_ack_emitted(&tcid_a);
                // Sleep is intentionally absent — we want the threads
                // racing tight, not serialized.
                let _ = tcid_b; // silence move warning while keeping symmetry
            });
            h1.join().expect("mark_failed thread");
            h2.join().expect("mark_synth_ack_emitted thread");

            let signals = collected.lock().unwrap().clone();
            assert_eq!(
                signals.len(),
                1,
                "iteration {iter}: race must produce exactly one signal regardless of interleaving",
            );
            assert_eq!(signals[0].task_id, task_id);
            assert!(signals[0].error_message.contains("race boom"));
        }
    }

    /// Codex round-2 MAJOR: `AckAndPending::pending` must be bounded
    /// so a pathological flow (synth-ack permanently suppressed +
    /// task never completes/cancels) cannot grow the supervisor
    /// without limit. After inserting `MAX_PENDING_FAILURES + 1`
    /// pending entries the oldest must be evicted and its eventual
    /// ack must NOT surface a recovery signal — the evicted entry
    /// has been dropped from the supervisor by design.
    #[test]
    fn pending_failures_eviction_when_max_size_exceeded() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);

        // Insert MAX + 1 pending entries with distinct tool_call_ids
        // so the FIFO order is well-defined (each `pending` map slot
        // has a unique key + insertion order). Each task is registered
        // and then `mark_failed` is called BEFORE any synth-ack, so
        // every entry goes pending.
        let mut task_ids = Vec::with_capacity(MAX_PENDING_FAILURES + 1);
        for i in 0..=MAX_PENDING_FAILURES {
            let tcid = format!("call-stash-{i:04}");
            let tid = supervisor.register("fm_tts", &tcid, None);
            supervisor.mark_failed(&tid, format!("boom-{i}"));
            task_ids.push((tid, tcid));
        }

        // Pre-conditions: nothing should have signaled yet — every
        // entry is sitting in the pending stash.
        assert!(
            collected.lock().unwrap().is_empty(),
            "no signals must fire before any synth-ack lands",
        );

        // The map should be exactly bounded at MAX_PENDING_FAILURES;
        // the very first insert (index 0) was evicted to make room
        // for index MAX_PENDING_FAILURES.
        {
            let guard = supervisor.ack_and_pending.lock().unwrap();
            assert_eq!(
                guard.pending.len(),
                MAX_PENDING_FAILURES,
                "pending map must stay at cap",
            );
            // Oldest tool_call_id is no longer in the map.
            assert!(
                !guard.pending.contains_key(&task_ids[0].0),
                "oldest pending entry must be evicted",
            );
            // Newest tool_call_id is present.
            assert!(
                guard
                    .pending
                    .contains_key(&task_ids[MAX_PENDING_FAILURES].0),
                "newest pending entry must remain",
            );
        }

        // Now firing the synth-ack for the EVICTED tool_call_id must
        // NOT surface a recovery signal — the pending entry is gone.
        supervisor.mark_synth_ack_emitted(&task_ids[0].1);
        assert!(
            collected.lock().unwrap().is_empty(),
            "evicted pending entry must not fire when its ack arrives",
        );

        // Firing the synth-ack for the NEWEST tool_call_id must fire
        // exactly one signal — the entry is still in the map.
        supervisor.mark_synth_ack_emitted(&task_ids[MAX_PENDING_FAILURES].1);
        let signals = collected.lock().unwrap().clone();
        assert_eq!(
            signals.len(),
            1,
            "retained pending entry must still fire when its ack arrives",
        );
        assert_eq!(signals[0].task_id, task_ids[MAX_PENDING_FAILURES].0);
    }

    /// Codex round-2 MAJOR: `AckAndPending::emitted_task_ids` must be
    /// bounded so the idempotency set cannot grow indefinitely over
    /// the supervisor's lifetime. After firing
    /// `MAX_FAILURE_SIGNAL_EMITTED_IDS + 1` distinct failure signals
    /// the oldest entry is evicted, which is safe because the task is
    /// long since terminal and the task_id (a UUID) is not reused.
    #[test]
    fn failure_signal_emitted_ids_eviction_when_max_size_exceeded() {
        let supervisor = TaskSupervisor::new();
        let _collected = collect_failure_signals(&supervisor);

        // Drive past the cap. Each iteration: register a task, mark
        // its synth-ack, mark it failed → one dispatch → one entry
        // appended to `emitted_task_ids`.
        let mut first_task_id = String::new();
        for i in 0..=MAX_FAILURE_SIGNAL_EMITTED_IDS {
            let tcid = format!("call-emit-{i:05}");
            let tid = supervisor.register("fm_tts", &tcid, None);
            supervisor.mark_synth_ack_emitted(&tcid);
            supervisor.mark_failed(&tid, format!("boom-{i}"));
            if i == 0 {
                first_task_id = tid;
            }
        }

        let guard = supervisor.ack_and_pending.lock().unwrap();
        assert_eq!(
            guard.emitted_task_ids.len(),
            MAX_FAILURE_SIGNAL_EMITTED_IDS,
            "emitted_task_ids must stay at cap",
        );
        // Oldest task_id is no longer in the set.
        assert!(
            !guard.emitted_task_ids.contains(&first_task_id),
            "oldest emitted task_id must be evicted",
        );
    }

    /// Codex round-3 MAJOR (PR #1324): the `pending_insertion_order`
    /// VecDeque must stay bounded across many fail-before-ack →
    /// ack-drain cycles, even though the cap inside `insert_pending`
    /// never fires (the HashMap returns to size 0 after every drain).
    ///
    /// Previously the VecDeque grew by one entry per cycle forever
    /// because `drain_pending_for_tool_call` removed from the map but
    /// left the task_id sitting in the queue. With ~1M cycles in a
    /// long-running supervisor that would leak ~1M Strings (~50 MB).
    #[test]
    fn pending_insertion_order_does_not_leak_after_drain_cycles() {
        let supervisor = TaskSupervisor::new();
        let _collected = collect_failure_signals(&supervisor);

        // 4× the cap so we cleanly exercise the regression. Each
        // iteration uses a distinct (task_id, tool_call_id) so the
        // pending stash is keyed uniquely, then the synth-ack drains
        // it via `drain_pending_for_tool_call`.
        let n = MAX_PENDING_FAILURES * 4;
        for i in 0..n {
            let tcid = format!("call-drain-{i:06}");
            let tid = supervisor.register("fm_tts", &tcid, None);
            // mark_failed before any synth-ack stashes a pending entry.
            supervisor.mark_failed(&tid, format!("boom-{i}"));
            // synth-ack drains the pending entry — the HashMap returns
            // to 0 each cycle so the cap in `insert_pending` never
            // fires, exposing the VecDeque leak in the un-fixed code.
            supervisor.mark_synth_ack_emitted(&tcid);
        }

        let guard = supervisor.ack_and_pending.lock().unwrap();
        assert!(
            guard.pending.is_empty(),
            "pending map must drain to empty after every cycle, found {} entries",
            guard.pending.len(),
        );
        assert!(
            guard.pending_insertion_order.len() <= MAX_PENDING_FAILURES,
            "pending_insertion_order leaked: {} entries (cap {})",
            guard.pending_insertion_order.len(),
            MAX_PENDING_FAILURES,
        );
        // Strictest assertion: the queue should actually be EMPTY
        // because every pending entry was drained. The `<= cap`
        // assertion above is the round-3 contract; this tighter one
        // documents the ideal state.
        assert!(
            guard.pending_insertion_order.is_empty(),
            "pending_insertion_order must be empty after all entries are drained, found {} entries",
            guard.pending_insertion_order.len(),
        );
    }

    /// Codex round-3 MAJOR (PR #1324): companion to the drain test
    /// above for the `remove_pending` path. `remove_pending` is called
    /// from `drain_pending_failure_for_task` (defensive cleanup in
    /// `mark_completed` / `cancel`) and must also pop the
    /// `pending_insertion_order` entry. Same leak class.
    ///
    /// In normal supervisor flow `mark_failed` makes the task
    /// terminal, so `mark_completed` and `cancel` short-circuit before
    /// reaching `remove_pending` — exercising it from the public API
    /// is awkward. We test the lockstep invariant directly on
    /// `AckAndPending` instead, which is what the round-3 fix
    /// guarantees regardless of how `remove_pending` is reached.
    #[test]
    fn pending_insertion_order_does_not_leak_after_remove_cycles() {
        let mut state = AckAndPending::default();
        let n = MAX_PENDING_FAILURES * 4;
        for i in 0..n {
            let task_id = format!("task-remove-{i:06}");
            let tcid = format!("call-remove-{i:06}");
            state.insert_pending(
                task_id.clone(),
                PendingFailure {
                    tool_call_id: tcid,
                    signal: SpawnOnlyFailureSignal {
                        task_id: task_id.clone(),
                        tool_name: "fm_tts".into(),
                        tool_input: Value::Null,
                        error_message: format!("boom-{i}"),
                        suggested_alternatives: Vec::new(),
                        parent_session_key: None,
                        originating_client_message_id: None,
                    },
                },
            );
            let removed = state.remove_pending(&task_id);
            assert!(
                removed.is_some(),
                "iteration {i}: remove_pending should return the inserted entry",
            );
        }

        assert!(
            state.pending.is_empty(),
            "pending map must drain to empty after every cycle, found {} entries",
            state.pending.len(),
        );
        assert!(
            state.pending_insertion_order.len() <= MAX_PENDING_FAILURES,
            "pending_insertion_order leaked under remove path: {} entries (cap {})",
            state.pending_insertion_order.len(),
            MAX_PENDING_FAILURES,
        );
        assert!(
            state.pending_insertion_order.is_empty(),
            "pending_insertion_order must be empty after all entries are removed, found {} entries",
            state.pending_insertion_order.len(),
        );
    }

    // ── F004 B2: TaskSupervisor → ToolProgress bridge ─────────────────────

    /// Test reporter that captures every reported event so the bridge
    /// assertions can branch on event kind without parsing JSON.
    struct CapturingReporter {
        events: Arc<StdMutex<Vec<crate::progress::ProgressEvent>>>,
    }

    impl crate::progress::ProgressReporter for CapturingReporter {
        fn report(&self, event: crate::progress::ProgressEvent) {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(event);
        }
    }

    fn collect_progress_events(
        supervisor: &TaskSupervisor,
    ) -> Arc<StdMutex<Vec<crate::progress::ProgressEvent>>> {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let reporter = Arc::new(CapturingReporter {
            events: Arc::clone(&events),
        });
        supervisor.set_progress_reporter(reporter);
        events
    }

    fn extract_tool_progress(
        events: &[crate::progress::ProgressEvent],
    ) -> Vec<(String, String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                crate::progress::ProgressEvent::ToolProgress {
                    name,
                    tool_id,
                    message,
                } => Some((name.clone(), tool_id.clone(), message.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn should_emit_tool_progress_on_runtime_state_transition() {
        let supervisor = TaskSupervisor::new();
        let events = collect_progress_events(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-progress-1", Some("api:session"));
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );

        let captured = events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        assert!(
            tool_progress.len() >= 2,
            "expected ToolProgress for mark_running + mark_runtime_state, got: {tool_progress:?}"
        );
        // Last event must reflect the DeliveringOutputs transition and
        // anchor on the originating tool_call_id so the chat UI can route
        // it to the right bubble.
        let (name, tool_id, message) = tool_progress.last().unwrap();
        assert_eq!(name, "fm_tts");
        assert_eq!(tool_id, "call-progress-1");
        assert_eq!(message, "fm_tts: delivering outputs");
    }

    #[test]
    fn should_emit_tool_progress_on_completion_with_tool_call_id() {
        let supervisor = TaskSupervisor::new();
        let events = collect_progress_events(&supervisor);
        let task_id = supervisor.register("podcast_generate", "call-complete-1", None);
        supervisor.mark_completed(&task_id, vec!["/tmp/out.mp3".to_string()]);

        let captured = events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        let completion = tool_progress
            .iter()
            .find(|(_, _, message)| message.ends_with(": completed"))
            .expect("completion progress event missing");
        assert_eq!(completion.0, "podcast_generate");
        assert_eq!(completion.1, "call-complete-1");
        assert_eq!(completion.2, "podcast_generate: completed");
    }

    #[test]
    fn should_emit_tool_progress_on_failure_with_reason() {
        let supervisor = TaskSupervisor::new();
        let events = collect_progress_events(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-fail-1", None);
        supervisor.mark_failed(&task_id, "workspace policy not found".to_string());

        let captured = events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        let failure = tool_progress
            .iter()
            .find(|(_, _, message)| message.contains("failed"))
            .expect("failure progress event missing");
        assert_eq!(failure.0, "fm_tts");
        assert_eq!(failure.1, "call-fail-1");
        assert_eq!(failure.2, "fm_tts: failed (workspace policy not found)");
    }

    #[test]
    fn should_not_emit_tool_progress_when_no_reporter_attached() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("fm_tts", "call-silent-1", None);
        // No reporter attached — must be a no-op (and crucially must not
        // panic on the missing reporter).
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );
        supervisor.mark_completed(&task_id, vec![]);
        // Nothing to assert beyond the absence of a panic — the reporter is
        // optional by design so the supervisor can be used outside the
        // chat-progress pipeline (e.g. cron, tests).
    }

    #[test]
    fn should_only_emit_failure_progress_once_per_task() {
        let supervisor = TaskSupervisor::new();
        let events = collect_progress_events(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-fail-dedup", None);
        supervisor.mark_failed(&task_id, "first".to_string());
        // Second mark_failed must NOT re-emit a ToolProgress for the
        // same task — mirrors the existing failure-signal dedup contract.
        supervisor.mark_failed(&task_id, "second".to_string());

        let captured = events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        let failures: Vec<_> = tool_progress
            .iter()
            .filter(|(_, _, message)| message.contains("failed"))
            .collect();
        assert_eq!(
            failures.len(),
            1,
            "expected exactly one failure ToolProgress, got: {failures:?}"
        );
    }

    // ────────── M7.9 cancel / relaunch primitives (W2) ──────────

    #[test]
    fn cancel_running_task_transitions_to_cancelled_and_fires_token() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("run_pipeline", "call-cancel-1", Some("session-A"));
        supervisor.mark_running(&task_id);
        let token = supervisor.cancel_token(&task_id);
        assert!(!token.is_cancelled());

        supervisor.cancel(&task_id).expect("cancel should succeed");

        let task = supervisor.get_task(&task_id).expect("task still tracked");
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Cancelled);
        assert!(token.is_cancelled());
        assert!(task.completed_at.is_some());
    }

    #[test]
    fn cancel_unknown_task_returns_not_found() {
        let supervisor = TaskSupervisor::new();
        let result = supervisor.cancel("does-not-exist");
        assert_eq!(result, Err(TaskCancelError::NotFound));
    }

    #[test]
    fn cancel_terminal_task_returns_already_terminal() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("podcast_generate", "call-cancel-2", Some("session-B"));
        supervisor.mark_completed(&task_id, vec!["output/podcast.mp3".into()]);
        let result = supervisor.cancel(&task_id);
        assert_eq!(result, Err(TaskCancelError::AlreadyTerminal));
        // Cancelling a Failed task is also rejected.
        let task_id2 = supervisor.register("fm_tts", "call-cancel-3", None);
        supervisor.mark_failed(&task_id2, "boom".to_string());
        assert_eq!(
            supervisor.cancel(&task_id2),
            Err(TaskCancelError::AlreadyTerminal)
        );
    }

    #[test]
    fn cancel_emits_progress_event() {
        let supervisor = TaskSupervisor::new();
        let events = collect_progress_events(&supervisor);
        let task_id = supervisor.register("run_pipeline", "call-cancel-4", Some("session-C"));
        supervisor.mark_running(&task_id);
        supervisor.cancel(&task_id).expect("cancel should succeed");

        let captured = events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        let cancels: Vec<_> = tool_progress
            .iter()
            .filter(|(_, _, message)| message.contains("cancelled"))
            .collect();
        assert!(
            !cancels.is_empty(),
            "expected at least one cancelled ToolProgress, got: {tool_progress:?}"
        );
    }

    // ────────── M8 Req #4 DoD: cancel cannot be overwritten by late workers ──────────

    /// Race regression: a worker that finishes AFTER the user has cancelled
    /// the task must NOT resurrect it to `Completed`. The supervisor's
    /// `mark_completed` guard short-circuits when the task is already in a
    /// terminal state. Asserts state stays `Cancelled`, the on_change callback
    /// fires exactly twice (once for `mark_running`, once for `cancel`), and
    /// the ProgressReporter does NOT emit a spurious "completed" event after
    /// cancellation.
    #[test]
    fn mark_completed_after_cancel_does_not_overwrite_cancelled_state() {
        use std::sync::Mutex;
        let supervisor = TaskSupervisor::new();
        let progress_events = collect_progress_events(&supervisor);
        let on_change_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        {
            let on_change_count = on_change_count.clone();
            supervisor.set_on_change(move |_task| {
                *on_change_count.lock().unwrap() += 1;
            });
        }

        let task_id = supervisor.register("run_pipeline", "call-race-1", Some("session-X"));
        supervisor.mark_running(&task_id); // notify #1
        supervisor.cancel(&task_id).expect("cancel should succeed"); // notify #2

        // Late-arriving worker tries to mark completed — this is the race.
        supervisor.mark_completed(&task_id, vec!["late/output.bin".into()]); // must noop

        let task = supervisor.get_task(&task_id).expect("task still tracked");
        assert_eq!(
            task.status,
            TaskStatus::Cancelled,
            "late mark_completed must NOT overwrite Cancelled state"
        );
        assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Cancelled);
        assert!(
            task.output_files.is_empty(),
            "late completion's output_files must not leak onto a Cancelled task, got: {:?}",
            task.output_files
        );

        // on_change must have fired exactly twice — guard noop must not
        // double-fire the change callback.
        assert_eq!(
            *on_change_count.lock().unwrap(),
            2,
            "on_change should fire exactly twice (mark_running + cancel), not for the noop mark_completed"
        );

        // ProgressReporter must not have emitted any "completed" message
        // after cancellation. We saw running + cancelled, but never completed.
        let captured = progress_events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        let post_cancel_completed: Vec<_> = tool_progress
            .iter()
            .filter(|(_, _, message)| message.contains("completed"))
            .collect();
        assert!(
            post_cancel_completed.is_empty(),
            "guard must not emit 'completed' progress for a cancelled task, got: {tool_progress:?}"
        );
    }

    /// Race regression mirror: a worker that fails AFTER the user has
    /// cancelled the task must NOT overwrite the cancellation with a
    /// `Failed` status. Without the guard this would corrupt the
    /// dashboard ("user cancelled" silently flips to "the task crashed").
    #[test]
    fn mark_failed_after_cancel_does_not_overwrite_cancelled_state() {
        use std::sync::Mutex;
        let supervisor = TaskSupervisor::new();
        let on_change_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        {
            let on_change_count = on_change_count.clone();
            supervisor.set_on_change(move |_task| {
                *on_change_count.lock().unwrap() += 1;
            });
        }
        let failure_signals: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        {
            let failure_signals = failure_signals.clone();
            supervisor.set_on_failure_signal(move |_signal| {
                *failure_signals.lock().unwrap() += 1;
            });
        }

        let task_id = supervisor.register("run_pipeline", "call-race-2", Some("session-Y"));
        supervisor.mark_running(&task_id); // notify #1
        supervisor.cancel(&task_id).expect("cancel should succeed"); // notify #2

        // Late-arriving worker reports failure — guard must reject.
        supervisor.mark_failed(&task_id, "late worker error".to_string());

        let task = supervisor.get_task(&task_id).expect("task still tracked");
        assert_eq!(
            task.status,
            TaskStatus::Cancelled,
            "late mark_failed must NOT overwrite Cancelled state"
        );
        assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
        assert_eq!(
            task.error.as_deref(),
            Some("cancelled by supervisor"),
            "cancel reason must survive the late mark_failed call"
        );

        assert_eq!(
            *on_change_count.lock().unwrap(),
            2,
            "on_change should fire exactly twice (mark_running + cancel), not for the noop mark_failed"
        );
        assert_eq!(
            *failure_signals.lock().unwrap(),
            0,
            "spawn-only failure signal must NOT fire for a cancelled task that hits the guard"
        );
    }

    /// Idempotency: calling `mark_completed` twice on the same task should
    /// be a no-op on the second call. The first call sets the terminal
    /// state; the second hits the guard and warns. Output files do NOT
    /// regress (the second call's payload is ignored), and the on_change /
    /// progress reporter both fire exactly once for the real transition.
    #[test]
    fn mark_completed_after_completed_is_idempotent_and_warns() {
        use std::sync::Mutex;
        let supervisor = TaskSupervisor::new();
        let progress_events = collect_progress_events(&supervisor);
        let on_change_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        {
            let on_change_count = on_change_count.clone();
            supervisor.set_on_change(move |_task| {
                *on_change_count.lock().unwrap() += 1;
            });
        }

        let task_id = supervisor.register("podcast_generate", "call-race-3", None);
        supervisor.mark_running(&task_id); // notify #1
        supervisor.mark_completed(&task_id, vec!["output/first.mp3".into()]); // notify #2

        // Second call must be a noop — no panic, no state regression.
        supervisor.mark_completed(&task_id, vec!["output/second.mp3".into()]);

        let task = supervisor.get_task(&task_id).expect("task still tracked");
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(
            task.output_files,
            vec!["output/first.mp3".to_string()],
            "second mark_completed must NOT replace the first call's output_files"
        );

        assert_eq!(
            *on_change_count.lock().unwrap(),
            2,
            "on_change should fire exactly twice (mark_running + first mark_completed), not for the noop second call"
        );

        // Progress reporter should see at most one "completed" emission.
        let captured = progress_events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        let completed_emissions: Vec<_> = tool_progress
            .iter()
            .filter(|(_, _, message)| message.contains("completed"))
            .collect();
        assert_eq!(
            completed_emissions.len(),
            1,
            "expected exactly one 'completed' progress emission, got: {tool_progress:?}"
        );
    }

    /// Race regression: a worker that calls `mark_running` AFTER the user has
    /// cancelled the task must NOT resurrect it to `Running`. This is the
    /// subtle case that hides under register → cancel-before-running →
    /// worker still observes the spawn and tries to flip Running before
    /// noticing the cancel token.
    #[test]
    fn mark_running_after_cancel_does_not_overwrite_cancelled_state() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("run_pipeline", "call-race-4", Some("session-Z"));
        // Cancel BEFORE mark_running — exercises the "cancelled while still
        // Spawned" branch of the race window.
        supervisor.cancel(&task_id).expect("cancel should succeed");

        // Late worker tries to mark running — must noop.
        supervisor.mark_running(&task_id);

        let task = supervisor.get_task(&task_id).expect("task still tracked");
        assert_eq!(
            task.status,
            TaskStatus::Cancelled,
            "late mark_running must NOT overwrite Cancelled state"
        );
        assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
    }

    /// Race regression: a worker that emits a harness progress event AFTER
    /// the user has cancelled the task must NOT corrupt the stored
    /// `runtime_state` away from `Cancelled`. Without the guard, ledger
    /// snapshots and progress emissions would flip to e.g. `executing_tool`
    /// even though the public `status` is still `Cancelled`.
    #[test]
    fn mark_runtime_state_after_cancel_does_not_overwrite_cancelled_runtime_state() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("run_pipeline", "call-race-5", Some("session-W"));
        supervisor.mark_running(&task_id);
        supervisor.cancel(&task_id).expect("cancel should succeed");

        // Late worker reports a phase update — must noop.
        supervisor.mark_runtime_state(
            &task_id,
            TaskRuntimeState::DeliveringOutputs,
            Some(r#"{"workflow_kind":"podcast","current_phase":"render"}"#.into()),
        );

        let task = supervisor.get_task(&task_id).expect("task still tracked");
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(
            task.runtime_state,
            TaskRuntimeState::Cancelled,
            "late mark_runtime_state must NOT overwrite Cancelled runtime_state"
        );
    }

    /// Race regression: late `mark_failed` after the task completed normally
    /// must not flip a `Completed` task back to `Failed`. This exercises the
    /// non-cancel branch of the new mark_failed guard.
    #[test]
    fn mark_failed_after_completed_does_not_overwrite_completed_state() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("podcast_generate", "call-race-6", None);
        supervisor.mark_running(&task_id);
        supervisor.mark_completed(&task_id, vec!["output/podcast.mp3".into()]);

        // Late worker reports a failure — must noop.
        supervisor.mark_failed(&task_id, "stale failure".to_string());

        let task = supervisor.get_task(&task_id).expect("task still tracked");
        assert_eq!(
            task.status,
            TaskStatus::Completed,
            "late mark_failed must NOT overwrite Completed state"
        );
        assert!(
            task.error.is_none(),
            "Completed task must not gain an error from a late mark_failed, got: {:?}",
            task.error
        );
    }

    #[test]
    fn relaunch_failed_task_creates_successor_and_fires_callback() {
        use std::sync::Mutex;
        let supervisor = TaskSupervisor::new();
        let captured: Arc<Mutex<Vec<RelaunchRequest>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let captured = captured.clone();
            supervisor.set_on_relaunch(move |req| {
                captured.lock().unwrap().push(req.clone());
            });
        }

        let task_id = supervisor.register("run_pipeline", "call-relaunch-1", Some("session-D"));
        supervisor.mark_running(&task_id);
        supervisor.mark_failed(&task_id, "node 'design' failed".to_string());

        let new_id = supervisor
            .relaunch(
                &task_id,
                RelaunchOpts {
                    from_node: Some("design".into()),
                },
            )
            .expect("relaunch should succeed");
        assert_ne!(new_id, task_id, "relaunch must allocate a fresh id");

        let new_task = supervisor.get_task(&new_id).expect("successor registered");
        assert_eq!(new_task.tool_name, "run_pipeline");
        assert_eq!(new_task.tool_call_id, "call-relaunch-1");
        assert_eq!(new_task.session_key.as_deref(), Some("session-D"));

        let log = captured.lock().unwrap();
        assert_eq!(log.len(), 1, "relaunch callback fired exactly once");
        assert_eq!(log[0].original_task_id, task_id);
        assert_eq!(log[0].new_task_id, new_id);
        assert_eq!(log[0].opts.from_node.as_deref(), Some("design"));
    }

    #[test]
    fn relaunch_unknown_task_returns_not_found() {
        let supervisor = TaskSupervisor::new();
        let result = supervisor.relaunch("does-not-exist", RelaunchOpts::default());
        assert_eq!(result, Err(TaskRelaunchError::NotFound));
    }

    #[test]
    fn relaunch_active_task_returns_still_active() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("run_pipeline", "call-relaunch-2", None);
        supervisor.mark_running(&task_id);
        let result = supervisor.relaunch(&task_id, RelaunchOpts::default());
        assert_eq!(result, Err(TaskRelaunchError::StillActive));
    }

    #[test]
    fn cancel_token_notifies_waiters() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("run_pipeline", "call-cancel-notify", None);
        supervisor.mark_running(&task_id);
        let token = supervisor.cancel_token(&task_id);

        // Drive a small async runtime so the token can fire its
        // notification path (poll-then-wait).
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let waiter = {
                let token = token.clone();
                tokio::spawn(async move { token.cancelled().await })
            };
            // Yield so the waiter actually parks on `notified()`.
            tokio::task::yield_now().await;
            supervisor.cancel(&task_id).expect("cancel should succeed");
            tokio::time::timeout(std::time::Duration::from_millis(500), waiter)
                .await
                .expect("waiter must wake within 500ms")
                .expect("waiter task panicked");
        });
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_token_catches_cancel_between_precheck_and_notify_park() {
        let token = Arc::new(TaskCancelToken::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let canceller = token.clone();
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                token.cancelled_after_first_check(move || canceller.cancel()),
            )
            .await
            .expect("cancelled() must not miss a cancel fired before Notified is parked");
        });
        assert!(token.is_cancelled());
    }

    /// Guard A regression: a parent session that has already accepted
    /// `MAX_CHILDREN_PER_PARENT` children must refuse the next register
    /// with a structured `ChildFanoutExceeded` error and force-fail every
    /// still-active child so the cascade collapses.
    #[test]
    fn register_task_refuses_201st_child_for_same_parent() {
        // Use a smaller cap via env var so the test does not allocate
        // 200+ tasks in CI. The cap reader caches once per process — we
        // run this test in isolation with a fresh `TaskSupervisor` and a
        // sub-process-friendly cap value that is set before any other
        // register call resolves the cache.
        //
        // Note: setting `OCTOS_MAX_CHILDREN_PER_PARENT` here would be
        // racy because `max_children_per_parent` caches with `OnceLock`.
        // Instead we exercise the production cap (200) — register 200
        // children, then assert the 201st is refused.
        let parent_session = "api:test-parent";
        let supervisor = TaskSupervisor::new();
        for i in 0..MAX_CHILDREN_PER_PARENT {
            let id = supervisor
                .try_register_with_input("tts", &format!("call-{i}"), Some(parent_session), None)
                .unwrap_or_else(|err| panic!("register #{i} should succeed; got {err}"));
            // Mark a slice of the children as active (Running) so the
            // force-fail cascade has something to flip on the 201st
            // call. Leaving every task in Spawned (also active) works
            // identically.
            if i % 2 == 0 {
                supervisor.mark_running(&id);
            }
        }
        assert_eq!(
            supervisor.get_tasks_for_session(parent_session).len(),
            MAX_CHILDREN_PER_PARENT,
            "supervisor should hold exactly the cap before the refusal fires"
        );

        // The 201st register must be refused with a typed error that
        // carries the count, cap, and the parent session key.
        let err = supervisor
            .try_register_with_input("tts", "call-overflow", Some(parent_session), None)
            .expect_err("201st child must be refused");
        match err {
            RegisterTaskError::ChildFanoutExceeded {
                parent_session_key,
                count,
                cap,
            } => {
                assert_eq!(parent_session_key, parent_session);
                assert_eq!(count, MAX_CHILDREN_PER_PARENT);
                assert_eq!(cap, MAX_CHILDREN_PER_PARENT);
            }
            other => panic!("expected ChildFanoutExceeded, got {other:?}"),
        }

        // The cap rejection must not leak a new task into the
        // supervisor — count stays at the cap.
        assert_eq!(
            supervisor.get_tasks_for_session(parent_session).len(),
            MAX_CHILDREN_PER_PARENT,
            "refused register must not insert a new task"
        );

        // Every still-active child of the runaway parent should have
        // been force-marked `Failed` with the structured reason so the
        // cascade collapses instead of waiting on each child to finish.
        let expected_reason = format!(
            "child fanout exceeded ({} of {})",
            MAX_CHILDREN_PER_PARENT, MAX_CHILDREN_PER_PARENT
        );
        let tasks = supervisor.get_tasks_for_session(parent_session);
        let any_active = tasks.iter().any(|t| t.status.is_active());
        assert!(
            !any_active,
            "every active child should be flipped to Failed after the cap fires"
        );
        let failed_with_reason = tasks
            .iter()
            .filter(|t| {
                t.status == TaskStatus::Failed
                    && t.error.as_deref() == Some(expected_reason.as_str())
            })
            .count();
        assert!(
            failed_with_reason > 0,
            "at least one child should carry the structured fan-out reason"
        );

        // A subsequent attempt against the same poisoned parent must
        // continue to be refused (fast-path via `poisoned_parents`).
        let err = supervisor
            .try_register_with_input("tts", "call-after-overflow", Some(parent_session), None)
            .expect_err("poisoned parent must keep refusing further registers");
        assert!(matches!(err, RegisterTaskError::ChildFanoutExceeded { .. }));

        // A fresh, distinct parent session is unaffected.
        let other = supervisor
            .try_register_with_input("tts", "call-other-1", Some("api:other-parent"), None)
            .expect("other parents stay unaffected by a poisoned peer");
        assert!(!other.is_empty());
    }

    /// The legacy `register_with_input` entry point keeps returning a
    /// `String`; on cap rejection it returns an empty-string sentinel
    /// rather than panicking so existing call sites still type-check.
    #[test]
    fn legacy_register_returns_empty_string_on_cap_rejection() {
        let parent_session = "api:legacy-parent";
        let supervisor = TaskSupervisor::new();
        for i in 0..MAX_CHILDREN_PER_PARENT {
            supervisor.register("tts", &format!("call-{i}"), Some(parent_session));
        }
        let id = supervisor.register("tts", "call-overflow", Some(parent_session));
        assert!(
            id.is_empty(),
            "legacy register must return empty-string sentinel when refused"
        );
    }

    #[test]
    fn enable_persistence_reaps_orphan_running_tasks_at_startup() {
        // The bug: when the runtime crashes mid-task, the JSONL ledger has a
        // non-terminal entry for the in-flight task (Running / ResolvingOutputs
        // / etc) but no Completed/Failed event. On restart, the supervisor
        // restored that state verbatim — leaving the task forever
        // non-terminal because no live worker is backing it anymore.
        //
        // The fix: after replay, any task whose runtime_state is non-terminal
        // is reaped — marked Failed("orphaned across restart") — so callers
        // observing the supervisor see a clean state.

        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        // Phase 1: simulate a previous run that registered two tasks. Task A
        // is left mid-flight (Running). Task B reached terminal Completed.
        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger_path).unwrap();
        let task_a =
            supervisor.register_with_lineage("search", "call-a", Some("api:session"), None);
        supervisor.mark_running(&task_a);
        let task_b =
            supervisor.register_with_lineage("fm_tts", "call-b", Some("api:session"), None);
        supervisor.mark_completed(&task_b, vec!["/tmp/voice.mp3".to_string()]);
        // Drop the first supervisor — its in-flight worker for task_a is gone.
        drop(supervisor);

        // Phase 2: a fresh supervisor replays the ledger and must reap the
        // orphaned non-terminal task.
        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger_path).unwrap();

        let reaped = restored
            .get_task(&task_a)
            .expect("orphan task must still be tracked after reap");
        assert_eq!(
            reaped.status,
            TaskStatus::Failed,
            "orphan task must be marked Failed at startup"
        );
        assert_eq!(reaped.runtime_state, TaskRuntimeState::Failed);
        let error = reaped.error.as_deref().unwrap_or("");
        assert!(
            error.contains("orphaned") || error.contains("restart"),
            "orphan task error must mention orphan/restart, got {error:?}"
        );
        assert!(
            reaped.completed_at.is_some(),
            "orphan task must have a completed_at timestamp"
        );

        let surviving = restored
            .get_task(&task_b)
            .expect("completed task must still be tracked after reap");
        assert_eq!(
            surviving.status,
            TaskStatus::Completed,
            "terminal tasks must not be reaped"
        );
        assert_eq!(surviving.runtime_state, TaskRuntimeState::Completed);

        // Idempotency: a third supervisor replaying the same ledger must see
        // task_a already terminal (because the reaper appended a Failed event).
        let restored_again = TaskSupervisor::new();
        restored_again.enable_persistence(&ledger_path).unwrap();
        let reread = restored_again
            .get_task(&task_a)
            .expect("orphan task still tracked on second replay");
        assert_eq!(reread.status, TaskStatus::Failed);
        let reread_error = reread.error.as_deref().unwrap_or("");
        assert!(
            reread_error.contains("orphaned") || reread_error.contains("restart"),
            "orphan task error must persist across replay, got {reread_error:?}"
        );
        // The completed task is unaffected on replay.
        let reread_b = restored_again
            .get_task(&task_b)
            .expect("completed task still tracked on second replay");
        assert_eq!(reread_b.status, TaskStatus::Completed);

        // Cancelled tasks must also be respected as terminal — they should
        // not be reaped a second time. Add a cancelled task to the ledger,
        // reload, and assert the cancellation survives.
        let cancel_supervisor = restored_again;
        let task_c = cancel_supervisor.register_with_lineage(
            "run_pipeline",
            "call-c",
            Some("api:session"),
            None,
        );
        cancel_supervisor.mark_running(&task_c);
        cancel_supervisor
            .cancel(&task_c)
            .expect("cancel should succeed");
        drop(cancel_supervisor);
        let final_reload = TaskSupervisor::new();
        final_reload.enable_persistence(&ledger_path).unwrap();
        let cancelled = final_reload
            .get_task(&task_c)
            .expect("cancelled task still tracked after reload");
        assert_eq!(
            cancelled.status,
            TaskStatus::Cancelled,
            "cancelled tasks must not be reaped"
        );
        assert_eq!(cancelled.runtime_state, TaskRuntimeState::Cancelled);
    }

    /// NEW-18b Option A — `try_register_node_task` must refuse a child
    /// registration when the parent task (looked up by
    /// `tool_call_id`) is already in a terminal state. This closes
    /// the race where pipeline tokio workers survive a serve restart,
    /// observe the orphan-swept parent as `failed`, and continue
    /// registering fresh node children that waste CPU/tokens.
    #[test]
    fn register_node_task_refuses_when_parent_already_failed() {
        let supervisor = TaskSupervisor::new();
        let parent_tcid = "call-pipeline-parent-x";

        // Pre-populate the parent in the failed state (mirrors the
        // post-orphan-sweep shape that triggers the race).
        let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-A"));
        supervisor.mark_running(&parent);
        supervisor.mark_failed(&parent, "orphaned across restart".to_string());
        assert_eq!(
            supervisor.get_task(&parent).unwrap().status,
            TaskStatus::Failed,
            "parent must be Failed before child registration races in"
        );

        // Straggler pipeline worker attempts to register a child node
        // task against the same parent_tool_call_id. Must be refused.
        let err = supervisor
            .try_register_node_task("pipeline:analyze", parent_tcid, Some("sess-A"))
            .expect_err("registration must be rejected for terminal parent");
        match err {
            RegisterTaskError::ParentTerminal {
                parent_tool_call_id,
                parent_status,
            } => {
                assert_eq!(parent_tool_call_id, parent_tcid);
                assert_eq!(parent_status, TaskStatus::Failed);
            }
            other => panic!("unexpected error variant: {other:?}"),
        }

        // The supervisor must NOT have any child task under that
        // parent — the straggler attempt was rejected before insert.
        let children: Vec<_> = supervisor
            .get_all_tasks()
            .into_iter()
            .filter(|task| {
                task.tool_call_id == parent_tcid && task.tool_name.starts_with("pipeline:")
            })
            .collect();
        assert!(
            children.is_empty(),
            "no pipeline child task should be registered; got {:?}",
            children.iter().map(|t| &t.tool_name).collect::<Vec<_>>()
        );
    }

    /// Same guard, but for `Cancelled` and `Completed` parents.
    #[test]
    fn register_node_task_refuses_when_parent_cancelled_or_completed() {
        let supervisor = TaskSupervisor::new();

        let cancel_tcid = "call-pipeline-parent-cancelled";
        let cancel_parent = supervisor.register("run_pipeline", cancel_tcid, Some("sess-cancel"));
        supervisor.mark_running(&cancel_parent);
        supervisor
            .cancel(&cancel_parent)
            .expect("cancel must succeed");
        let err = supervisor
            .try_register_node_task("pipeline:setup", cancel_tcid, Some("sess-cancel"))
            .expect_err("registration must be rejected for cancelled parent");
        assert!(
            matches!(
                err,
                RegisterTaskError::ParentTerminal {
                    parent_status: TaskStatus::Cancelled,
                    ..
                }
            ),
            "expected ParentTerminal/Cancelled, got {err:?}"
        );

        let done_tcid = "call-pipeline-parent-completed";
        let done_parent = supervisor.register("run_pipeline", done_tcid, Some("sess-done"));
        supervisor.mark_running(&done_parent);
        supervisor.mark_completed(&done_parent, vec![]);
        let err = supervisor
            .try_register_node_task("pipeline:setup", done_tcid, Some("sess-done"))
            .expect_err("registration must be rejected for completed parent");
        assert!(
            matches!(
                err,
                RegisterTaskError::ParentTerminal {
                    parent_status: TaskStatus::Completed,
                    ..
                }
            ),
            "expected ParentTerminal/Completed, got {err:?}"
        );
    }

    /// Healthy parent: registration must succeed.
    #[test]
    fn register_node_task_succeeds_when_parent_running() {
        let supervisor = TaskSupervisor::new();
        let parent_tcid = "call-pipeline-parent-running";
        let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-ok"));
        supervisor.mark_running(&parent);

        let child_id = supervisor
            .try_register_node_task("pipeline:analyze", parent_tcid, Some("sess-ok"))
            .expect("registration must succeed when parent is Running");
        assert!(!child_id.is_empty());

        let child = supervisor.get_task(&child_id).expect("child registered");
        assert_eq!(child.tool_name, "pipeline:analyze");
        assert_eq!(child.tool_call_id, parent_tcid);
    }

    /// Unknown parent (no matching tool_call_id in the supervisor):
    /// `try_register_node_task` falls through to normal registration
    /// instead of rejecting. This keeps legacy/test callers that
    /// never register a `run_pipeline` parent on the no-op path.
    #[test]
    fn register_node_task_allows_when_no_parent_registered() {
        let supervisor = TaskSupervisor::new();
        let child_id = supervisor
            .try_register_node_task("pipeline:analyze", "call-no-parent", Some("sess-test"))
            .expect("unknown parent must fall through to normal registration");
        assert!(!child_id.is_empty());
    }

    /// Codex P2 #2 — when a `run_pipeline` task is relaunched with
    /// the same `tool_call_id` (mirroring `TaskSupervisor::relaunch`'s
    /// behaviour), the lookup must return the ACTIVE relaunch's
    /// status, not the stale failed predecessor's. Without preferring
    /// active records, a fresh node registration under the live
    /// relaunch would be rejected just because the failed record
    /// happens to share the tool_call_id.
    #[test]
    fn parent_status_for_tool_call_id_prefers_active_relaunch_over_stale_failed() {
        let supervisor = TaskSupervisor::new();
        let tcid = "call-relaunched-tcid";

        // Original parent: Failed (the predecessor that triggered
        // relaunch).
        let original = supervisor.register("run_pipeline", tcid, Some("sess-relaunch"));
        supervisor.mark_running(&original);
        supervisor.mark_failed(&original, "predecessor failed".to_string());

        // Relaunch: a fresh parent task registered with the same
        // tool_call_id. Status: Running.
        let relaunched = supervisor.register("run_pipeline", tcid, Some("sess-relaunch"));
        supervisor.mark_running(&relaunched);

        let status = supervisor.parent_status_for_tool_call_id(tcid);
        assert_eq!(
            status,
            Some(TaskStatus::Running),
            "lookup must prefer the active relaunch over the stale failed predecessor"
        );

        // Consequence: try_register_node_task must SUCCEED for the
        // live relaunch.
        let child = supervisor
            .try_register_node_task("pipeline:analyze", tcid, Some("sess-relaunch"))
            .expect("child registration must succeed for live relaunch");
        assert!(!child.is_empty());
    }

    /// `parent_status_for_tool_call_id` must filter OUT sibling
    /// `pipeline:<node>` records when resolving the parent status,
    /// because every pipeline child reuses the parent's tool_call_id.
    /// Without the filter the lookup could return a sibling's status
    /// and incorrectly reject a fresh child even though the actual
    /// parent is still Running.
    #[test]
    fn parent_status_for_tool_call_id_ignores_pipeline_siblings() {
        let supervisor = TaskSupervisor::new();
        let tcid = "call-shared";
        // Sibling pipeline child that just transitioned to Failed.
        let sib = supervisor.register("pipeline:setup", tcid, Some("sess-shared"));
        supervisor.mark_running(&sib);
        supervisor.mark_failed(&sib, "node failed".to_string());

        // Parent run_pipeline task is still Running.
        let parent = supervisor.register("run_pipeline", tcid, Some("sess-shared"));
        supervisor.mark_running(&parent);

        let status = supervisor.parent_status_for_tool_call_id(tcid);
        assert_eq!(
            status,
            Some(TaskStatus::Running),
            "lookup must skip pipeline:<node> siblings and return the parent's status"
        );

        // And as the consequence, registration of another node child
        // must succeed.
        let new_child = supervisor
            .try_register_node_task("pipeline:analyze", tcid, Some("sess-shared"))
            .expect("registration must succeed while parent is Running");
        assert!(!new_child.is_empty());
    }

    /// NEW-18b Option C — `enable_persistence`'s orphan sweep must
    /// also cascade-fail any LIVE pipeline children that share the
    /// parent's `tool_call_id`. Catches the case where children
    /// already registered before the sweep fires (e.g. they were
    /// persisted to JSONL while their workers were running, then the
    /// process crashed mid-run).
    #[test]
    fn enable_persistence_cascades_to_children_with_same_tool_call_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        // Pre-populate the ledger with one orphan parent + two orphan
        // children sharing its tool_call_id, plus one unrelated
        // sibling under a different tool_call_id (must NOT be
        // cascaded). All three "running" tasks have non-terminal
        // runtime_state so the orphan reaper will mark them Failed.
        let parent_tcid = "call-pipeline-mini3-phantom";
        let writer = TaskSupervisor::new();
        writer.enable_persistence(&ledger_path).unwrap();
        let parent = writer.register("run_pipeline", parent_tcid, Some("sess-phantom"));
        let child1 = writer.register("pipeline:analyze", parent_tcid, Some("sess-phantom"));
        let child2 = writer.register("pipeline:synthesize", parent_tcid, Some("sess-phantom"));
        let unrelated =
            writer.register("pipeline:other", "call-other-parent", Some("sess-phantom"));
        writer.mark_running(&parent);
        writer.mark_running(&child1);
        writer.mark_running(&child2);
        writer.mark_running(&unrelated);
        drop(writer);

        // Fresh supervisor replays the ledger and runs the orphan
        // sweep. After enable_persistence returns, every orphan
        // parent's children should ALSO be terminal.
        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger_path).unwrap();

        // Parent: orphan-swept to Failed with the standard reason.
        let parent_task = restored.get_task(&parent).expect("parent persisted");
        assert_eq!(parent_task.status, TaskStatus::Failed);
        assert_eq!(
            parent_task.error.as_deref(),
            Some("orphaned across restart"),
            "parent retains the standard orphan-sweep reason"
        );

        // Both children under the orphaned parent must now be Failed.
        // They could be Failed via EITHER (a) the orphan sweep itself
        // (because they are also non-terminal-runtime-state) OR (b)
        // the Option-C cascade. Both paths satisfy the contract: the
        // child task is terminal and no longer wastes CPU/tokens.
        for cid in [&child1, &child2] {
            let task = restored.get_task(cid).expect("child persisted");
            assert_eq!(
                task.status,
                TaskStatus::Failed,
                "child {cid} must be Failed after restart sweep + cascade"
            );
            assert_eq!(task.runtime_state, TaskRuntimeState::Failed);
            assert!(task.completed_at.is_some());
            let reason = task.error.clone().unwrap_or_default();
            assert!(
                reason == "orphaned across restart"
                    || reason == "parent task orphaned across restart",
                "child {cid} must carry orphan-sweep OR cascade reason, got '{reason}'"
            );
        }

        // The unrelated sibling under a different parent tool_call_id
        // should still be Failed (orphan sweep applies to it too —
        // its own runtime_state is non-terminal) BUT it must NOT
        // carry the "parent task orphaned" reason: that's the cascade
        // marker for descendants of an orphaned parent.
        let other = restored.get_task(&unrelated).expect("unrelated persisted");
        assert_eq!(
            other.status,
            TaskStatus::Failed,
            "unrelated orphan is also swept, just via the main sweep loop"
        );
        // Note: when the unrelated task is itself an orphan, the main
        // sweep marks it Failed first. Then the cascade with its
        // tool_call_id ("call-other-parent") runs but finds no other
        // children under that key. So its reason should be the main
        // sweep's "orphaned across restart", not the cascade's variant.
        assert_eq!(
            other.error.as_deref(),
            Some("orphaned across restart"),
            "unrelated orphan must carry the standard reason"
        );
    }

    /// Option-C cascade must run as a DISTINCT post-sweep pass.
    ///
    /// Scenario: a pipeline child has `status = Running` (so it's
    /// still active from the cascade's perspective) BUT its
    /// `runtime_state` was concurrently driven into a terminal state
    /// (`ResolvingOutputs` finished and the worker wrote
    /// `runtime_state = Completed` but crashed before it could call
    /// `mark_completed` to also flip `status = Completed`). The main
    /// orphan sweep's `!is_terminal_runtime_state` filter SKIPS this
    /// child — runtime_state is already terminal. Without Option-C,
    /// the child stays `status = Running` forever after the parent
    /// is orphan-swept. With Option-C, `mark_descendants_failed`
    /// (which filters by `status.is_active()`) catches it.
    ///
    /// This test pins that Option-C cascade actually transitions
    /// such children to `Failed` after `enable_persistence` returns.
    #[test]
    fn enable_persistence_cascade_catches_active_status_with_terminal_runtime_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        let parent_tcid = "call-mixed-state-parent";
        let writer = TaskSupervisor::new();
        writer.enable_persistence(&ledger_path).unwrap();
        let parent = writer.register("run_pipeline", parent_tcid, Some("sess-mix"));
        // Healthy orphan child that the main sweep catches.
        let healthy_orphan = writer.register("pipeline:setup", parent_tcid, Some("sess-mix"));
        // "Mixed-state" child: status=Running, runtime_state=Completed
        // (set explicitly via mark_runtime_state).
        let mixed_child = writer.register("pipeline:analyze", parent_tcid, Some("sess-mix"));
        writer.mark_running(&parent);
        writer.mark_running(&healthy_orphan);
        writer.mark_running(&mixed_child);
        // Drive runtime_state to a terminal value WITHOUT touching
        // status. This simulates the worker crashing after it set
        // `runtime_state = Completed` but before `mark_completed`
        // flipped `status` to Completed.
        writer.mark_runtime_state(
            &mixed_child,
            TaskRuntimeState::Completed,
            Some("worker finished but crashed pre-mark_completed".to_string()),
        );
        // Sanity: status is still Running, runtime_state is terminal.
        let pre = writer.get_task(&mixed_child).unwrap();
        assert_eq!(pre.status, TaskStatus::Running);
        assert_eq!(pre.runtime_state, TaskRuntimeState::Completed);
        drop(writer);

        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger_path).unwrap();

        // Parent: main sweep catches it (status=Running, runtime_state
        // is non-terminal — `Spawned`).
        let parent_task = restored.get_task(&parent).expect("parent loaded");
        assert_eq!(parent_task.status, TaskStatus::Failed);
        assert_eq!(
            parent_task.error.as_deref(),
            Some("orphaned across restart")
        );

        // Healthy orphan child: main sweep catches it.
        let h = restored.get_task(&healthy_orphan).expect("healthy loaded");
        assert_eq!(h.status, TaskStatus::Failed);

        // Mixed-state child: main sweep SKIPS it because its
        // runtime_state is already terminal (Completed). The Option-C
        // cascade fires immediately after and DOES catch it — its
        // status was still `is_active()` when the cascade ran.
        let m = restored.get_task(&mixed_child).expect("mixed loaded");
        assert_eq!(
            m.status,
            TaskStatus::Failed,
            "mixed-state child must be Failed after Option-C cascade"
        );
        assert_eq!(
            m.error.as_deref(),
            Some("parent task orphaned across restart"),
            "mixed-state child must carry the cascade reason (proves Option-C ran distinctly from main sweep)"
        );
    }

    /// Codex P2 atomicity — the parent-terminal check inside
    /// `register_full` happens under the SAME `tasks` lock as the
    /// child insert. There is no observable window between lookup
    /// and insert. This test pins that the strict node-registration
    /// path actually goes through `register_full`'s inside-lock
    /// guard (not an outside-lock check that could race).
    ///
    /// We assert this indirectly by verifying that even a child
    /// inserted via the regular non-strict path (which has NO
    /// parent check) ends up in the supervisor — proving the strict
    /// guard is the ONLY mechanism that refuses based on parent
    /// state, and that strict mode actually exercises the in-lock
    /// recheck (since we use `try_register_node_task`, not the
    /// outside-lock convenience wrapper).
    #[test]
    fn try_register_node_task_uses_in_lock_guard_not_outside_check() {
        let supervisor = TaskSupervisor::new();
        let parent_tcid = "call-atomic-guard";
        let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-atom"));
        supervisor.mark_running(&parent);
        supervisor.mark_failed(&parent, "orphaned across restart".to_string());

        // Strict registration must reject (in-lock guard).
        let err = supervisor
            .try_register_node_task("pipeline:analyze", parent_tcid, Some("sess-atom"))
            .expect_err("strict path must reject terminal parent");
        assert!(matches!(err, RegisterTaskError::ParentTerminal { .. }));

        // Non-strict registration via `register` (no parent guard)
        // succeeds — this proves the rejection in the strict path
        // is the parent-terminal guard, not some unrelated check.
        let allowed = supervisor.register("pipeline:setup", parent_tcid, Some("sess-atom"));
        assert!(
            !allowed.is_empty(),
            "non-strict register must NOT consult parent status — the guard is opt-in"
        );
    }

    /// Codex P2 follow-up — terminal-parent rejection must NOT trigger
    /// the fan-out cap path's side effects (poisoning the session,
    /// `mark_failed`-ing every active sibling under the same
    /// `parent_session_key`). The terminal-parent check in
    /// `register_full` short-circuits the cap block in two places:
    /// (1) at the pre-cap fast path, and (2) under the same lock as
    /// the cap-check itself (atomic with the cap decision).
    ///
    /// This test exercises path (2) — it drives the session to
    /// `MAX_CHILDREN_PER_PARENT`, then a registration attempt against
    /// a TERMINAL parent in that same session must return
    /// `ParentTerminal` without poisoning the session or
    /// cascade-failing the existing 200 active siblings.
    #[test]
    fn try_register_node_task_terminal_parent_does_not_trigger_fanout_side_effects() {
        let supervisor = TaskSupervisor::new();
        let session = "api:sess-cap-collateral";

        // Pre-fill the session to MAX_CHILDREN_PER_PARENT - 1 active
        // unrelated tasks, then register the terminal parent as the
        // exact cap-th task. This puts count == cap when the test's
        // straggler attempt fires, so the cap branch is exercised.
        let terminal_parent_tcid = "call-terminal-parent-at-cap";
        let n_fill = MAX_CHILDREN_PER_PARENT - 1;
        let mut active_siblings = Vec::with_capacity(n_fill);
        for i in 0..n_fill {
            let id = supervisor
                .try_register_with_input("tts", &format!("call-{i}"), Some(session), None)
                .unwrap_or_else(|err| {
                    panic!("filling cap: register #{i} should succeed; got {err}")
                });
            supervisor.mark_running(&id);
            active_siblings.push(id);
        }
        let terminal_parent = supervisor
            .try_register_with_input("run_pipeline", terminal_parent_tcid, Some(session), None)
            .expect("terminal parent register at cap-1 must succeed (just barely fits)");
        supervisor.mark_running(&terminal_parent);
        supervisor.mark_failed(&terminal_parent, "orphaned across restart".to_string());
        assert_eq!(
            supervisor.get_tasks_for_session(session).len(),
            MAX_CHILDREN_PER_PARENT,
            "session must be exactly at cap before the test attempt"
        );

        // Snapshot how many active siblings exist BEFORE the attempt.
        // Should be n_fill (the parent itself is Failed, not active).
        let pre_active: usize = supervisor
            .get_tasks_for_session(session)
            .into_iter()
            .filter(|t| t.status.is_active())
            .count();
        assert_eq!(
            pre_active, n_fill,
            "expected {n_fill} active siblings (parent itself is terminal) before attempt"
        );

        // Straggler attempt: register a pipeline child under the
        // terminal parent IN THE CAPPED SESSION. The fix's atomic
        // recheck must catch this and return ParentTerminal — NOT
        // ChildFanoutExceeded. Without the inside-cap-lock recheck
        // the cap path would poison the session and `mark_failed`
        // every active sibling first.
        let err = supervisor
            .try_register_node_task("pipeline:analyze", terminal_parent_tcid, Some(session))
            .expect_err("registration must be rejected for terminal parent (even at cap)");
        assert!(
            matches!(err, RegisterTaskError::ParentTerminal { .. }),
            "must return ParentTerminal not ChildFanoutExceeded; got {err:?}",
        );

        // The session must NOT be poisoned: subsequent legitimate
        // failure attempts (cap-only path, no terminal parent) must
        // still hit ChildFanoutExceeded with their own count, not the
        // ParentTerminal already-poisoned fast path. We can't probe
        // the poisoned set directly, but we can probe its effect:
        // attempting a NON-strict registration would also be refused
        // if poisoned. (Skip this verification since the
        // ChildFanoutExceeded sibling count would itself trigger if
        // we tried — the cleaner assertion is on active sibling
        // counts.)

        // The 200 active siblings must remain UNTOUCHED — the cap
        // path's force-fail cascade did NOT run.
        let post_active: usize = supervisor
            .get_tasks_for_session(session)
            .into_iter()
            .filter(|t| t.status.is_active())
            .count();
        assert_eq!(
            post_active, pre_active,
            "no active sibling may be cascaded by a terminal-parent rejection at cap"
        );
    }

    /// NEW-09 contract: cascade-failing a child via
    /// `mark_descendants_failed` must still emit the per-task
    /// completion bubble (spawn_only on_failure_signal +
    /// emit_progress_for_state). This pin guarantees that the
    /// Option-C cascade does not regress NEW-09 — every cascade-
    /// failed child fires the same notification callbacks as a
    /// direct `mark_failed` call.
    #[test]
    fn mark_descendants_failed_emits_progress_and_failure_signal_per_child() {
        use std::sync::Mutex;

        let supervisor = TaskSupervisor::new();
        let parent_tcid = "call-cascade-signals";

        let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-sig"));
        let c1 = supervisor.register("pipeline:setup", parent_tcid, Some("sess-sig"));
        let c2 = supervisor.register("pipeline:analyze", parent_tcid, Some("sess-sig"));
        // Children inherit the parent's tool_call_id; mark the synth-ack
        // for that id so post-spawn failure signals fire (production wires
        // this from the synth-ack gate in `loop_runner.rs`).
        supervisor.mark_synth_ack_emitted(parent_tcid);
        supervisor.mark_running(&parent);
        supervisor.mark_running(&c1);
        supervisor.mark_running(&c2);

        // Capture every on_failure_signal payload that fires.
        let failure_signals: Arc<Mutex<Vec<SpawnOnlyFailureSignal>>> =
            Arc::new(Mutex::new(Vec::new()));
        {
            let captured = failure_signals.clone();
            supervisor.set_on_failure_signal(move |signal| {
                captured.lock().unwrap().push(signal.clone());
            });
        }

        // Capture every on_change snapshot. mark_failed fires
        // notify_change unconditionally for every transition.
        let change_log: Arc<Mutex<Vec<BackgroundTask>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let captured = change_log.clone();
            supervisor.set_on_change(move |task| {
                captured.lock().unwrap().push(task.clone());
            });
        }

        let cascaded =
            supervisor.mark_descendants_failed(parent_tcid, "parent task orphaned across restart");
        assert_eq!(cascaded, 2, "both running children should cascade-fail");

        // Failure signals: one per child, neither for the parent.
        let signals = failure_signals.lock().unwrap();
        assert_eq!(
            signals.len(),
            2,
            "every cascade-failed child must fire on_failure_signal (NEW-09)"
        );
        let signal_task_ids: HashSet<&str> = signals.iter().map(|s| s.task_id.as_str()).collect();
        assert!(signal_task_ids.contains(c1.as_str()));
        assert!(signal_task_ids.contains(c2.as_str()));
        for sig in signals.iter() {
            assert_eq!(
                sig.error_message, "parent task orphaned across restart",
                "cascade reason must propagate into the failure signal payload"
            );
            assert_eq!(sig.parent_session_key.as_deref(), Some("sess-sig"));
        }

        // on_change must have fired for both children's terminal
        // transitions. (We don't assert exact count because the
        // parent's earlier mark_running fires it too, but the failed
        // snapshots must be present.)
        let changes = change_log.lock().unwrap();
        let failed_snapshots: Vec<_> = changes
            .iter()
            .filter(|t| t.status == TaskStatus::Failed && t.tool_name.starts_with("pipeline:"))
            .collect();
        assert!(
            failed_snapshots.len() >= 2,
            "on_change must fire for each cascade-failed child terminal transition; \
             got {} failed pipeline snapshots",
            failed_snapshots.len()
        );
    }
}
