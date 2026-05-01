//! UI Protocol v1 WebSocket transport.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::Extension;
use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use octos_agent::{Agent, ToolApprovalDecision, ToolApprovalRequest};
use octos_core::ui_protocol::{
    ApprovalAutoResolvedEvent, ApprovalCancelledEvent, ApprovalCommandDetails,
    ApprovalDecidedEvent, ApprovalDecision, ApprovalId, ApprovalRenderHints,
    ApprovalRequestedEvent, ApprovalTypedDetails, InputItem, MessageDeltaEvent, OutputCursor,
    ReplayLossyEvent, RpcError, RpcErrorResponse, RpcRequest, RpcResponse, SessionOpenParams,
    SessionOpenResult, SessionOpened, TaskCancelParams, TaskCancelResult, TaskListEntry,
    TaskListParams, TaskListResult, TaskOutputDeltaEvent, TaskRestartFromNodeParams,
    TaskRestartFromNodeResult, TaskRuntimeState as UiTaskRuntimeState, TaskUpdatedEvent,
    ToolCompletedEvent, ToolProgressEvent, ToolStartedEvent, TurnCompletedEvent, TurnErrorEvent,
    TurnId, TurnInterruptParams, TurnStartParams, UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1,
    UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1, UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
    UiArtifactPaneItem, UiArtifactPaneSnapshot, UiCommand, UiCursor, UiFileMutationNotice,
    UiGitHistoryItem, UiGitPaneSnapshot, UiGitStatusItem, UiNotification, UiPaneSnapshot,
    UiPaneSnapshotLimitation, UiProgressEvent, UiProgressMetadata, UiWorkspacePaneEntry,
    UiWorkspacePaneSnapshot, approval_cancelled_reasons, approval_kinds, progress_kinds,
};
use octos_core::{AgentId, MAIN_PROFILE_ID, Message, MessageRole, SessionKey, TaskId};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex as TokioMutex, mpsc, oneshot};
use tokio::task::AbortHandle;
use tracing::info;

use super::AppState;
use super::metrics::MetricsReporter;
use super::router::AuthIdentity;
use super::ui_protocol_approvals::PendingApprovalStore;
use super::ui_protocol_audit::{ApprovalsAuditConfig, ApprovalsAuditLog, log_decision_tracing};
use super::ui_protocol_diff::{DiffPreviewConfig, PendingDiffPreviewStore};
use super::ui_protocol_ledger::{
    LedgerConfig, LedgeredUiProtocolEvent, UiProtocolLedger, UiProtocolLedgerEvent,
    spawn_eviction_task,
};
use super::ui_protocol_progress::{
    ProgressMappingContext, UiProgressMapping, background_task_to_progress_json, map_progress_json,
};
use super::ui_protocol_sanitize::sanitize_display_path;
use super::ui_protocol_scope::{ApprovalScopeKind, ScopePolicy, match_key_for};
use super::ui_protocol_task_output;

const FRAME_TOO_LARGE: i64 = -32005;
const MAX_TEXT_FRAME_BYTES: usize = 1024 * 1024;
const MAX_DIFF_PREVIEW_BYTES: usize = 256 * 1024;
const PROGRESS_CHANNEL_CAPACITY: usize = 1024;
/// Wall-clock budget for delivering a *terminal* task lifecycle update
/// (`completed` / `failed` / `cancelled`) when the bounded progress
/// channel is full. Long enough that real WebSocket backpressure can
/// drain (UI repaint, network blip), short enough that we don't pile up
/// zombie sends if the consumer is permanently gone. See
/// `forward_task_progress_to_channel` for the durability contract.
const TERMINAL_TASK_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Per-session ring buffer cap. Bumped from 1024 (M9.6 default) to
/// 4096 in M9-FIX-05 — a tool-heavy turn was clipping the start of the
/// current turn from replay. Disk log is now the source of truth, so
/// this is the LRU hot-cache size, not the durable retention.
const EVENT_LEDGER_RETAINED_PER_SESSION: usize = 4096;
const UI_FEATURES_HEADER: &str = "x-octos-ui-features";
/// Spec §10 `unknown_turn` (M9-FIX-02 wires this into `RpcError::unknown_turn`).
/// Until that lands in the trunk this worktree is rebased on, we keep a local
/// constant so the wire code stays correct. TODO: link to M9-FIX-02 once merged.
const UNKNOWN_TURN_CODE: i64 = -32101;
/// Maximum time we wait for the turn task to acknowledge an interrupt before
/// returning `ack_timeout` to the caller.
const INTERRUPT_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Per-connection bounded channel for outgoing WS frames. Decouples send
/// callers from the actual socket so a slow client cannot wedge unrelated
/// traffic. Tunable per session size.
const WS_WRITER_CHANNEL_CAPACITY: usize = 1024;
const APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED: &str = "request_send_failed";
type WsSink = futures::stream::SplitSink<WebSocket, WsMessage>;
type SharedActiveTurns = Arc<tokio::sync::Mutex<HashMap<SessionKey, ActiveTurn>>>;
type SharedConnectionTurns = Arc<tokio::sync::Mutex<HashMap<SessionKey, TurnId>>>;

/// Outcome of pushing a frame onto the per-connection writer channel.
///
/// All cases are non-fatal at the channel layer; callers decide how to react.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendError {
    /// Channel is full. The frame was not enqueued. For durable notifications
    /// this triggers a `protocol/replay_lossy` summary; for ephemeral frames
    /// it is logged at DEBUG and dropped.
    BackpressureDrop,
    /// Writer task has exited (peer disconnected or socket error). No further
    /// sends will succeed on this connection.
    Closed,
    /// A lifecycle send (turn lifecycle, RPC reply) failed. The string carries
    /// a short reason for the calling turn to abort cleanly and mark the
    /// ledger entry `delivery_failed`.
    LifecycleFailure(String),
}

// Send-site categorization per M9-FIX-04 § Acceptance criteria:
//   • lifecycle  — RPC results/errors, turn/started, turn/completed,
//                  turn/error. Use `send_notification_lifecycle` /
//                  `send_rpc_*`; errors propagate; ledger entry stays as
//                  `delivery_failed`.
//   • durable    — tool/task/approval/warning. Use
//                  `send_notification_durable`; drops bump dropped_count
//                  and emit `protocol/replay_lossy`.
//   • ephemeral  — message/delta. Use `send_notification_ephemeral`;
//                  drops are silent (spec § 9).

#[derive(Debug, Default)]
pub(crate) struct ConnectionMetrics {
    pub(crate) dropped_count: AtomicU64,
    pub(crate) last_durable_seq: AtomicU64,
    pub(crate) last_durable_stream: tokio::sync::Mutex<Option<String>>,
}

impl ConnectionMetrics {
    fn record_durable_cursor(&self, cursor: &UiCursor) {
        self.last_durable_seq.store(cursor.seq, Ordering::Relaxed);
        if let Ok(mut stream) = self.last_durable_stream.try_lock() {
            *stream = Some(cursor.stream.clone());
        }
    }

    fn snapshot_last_cursor(&self) -> Option<UiCursor> {
        let seq = self.last_durable_seq.load(Ordering::Relaxed);
        if seq == 0 {
            return None;
        }
        let stream = self
            .last_durable_stream
            .try_lock()
            .ok()
            .and_then(|guard| guard.clone())?;
        Some(UiCursor { stream, seq })
    }
}

/// Per-connection writer handle: hands frames to a dedicated drainer task.
///
/// Replaces the old `Arc<Mutex<WsSink>>` pattern so no caller ever holds a
/// lock across the network `await`. Cloning is cheap; the underlying writer
/// task lives until the channel is closed (last sender dropped) or the sink
/// errors.
#[derive(Clone)]
pub(crate) struct WsConnection {
    writer: mpsc::Sender<WsMessage>,
    metrics: Arc<ConnectionMetrics>,
}

impl WsConnection {
    pub(crate) fn new(writer: mpsc::Sender<WsMessage>) -> Self {
        Self {
            writer,
            metrics: Arc::new(ConnectionMetrics::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn metrics(&self) -> Arc<ConnectionMetrics> {
        self.metrics.clone()
    }

    fn try_enqueue(&self, frame: WsMessage) -> Result<(), SendError> {
        // Update the queue-depth gauge whenever we touch the channel — cheap
        // and gives an accurate signal even when sends succeed.
        let depth = WS_WRITER_CHANNEL_CAPACITY.saturating_sub(self.writer.capacity());
        metrics::gauge!("ws.connection.queue_depth").set(depth as f64);
        match self.writer.try_send(frame) {
            Ok(_) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SendError::BackpressureDrop),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SendError::Closed),
        }
    }

    /// Lifecycle: turn lifecycle / RPC reply. Caller acts on the failure.
    fn send_lifecycle(&self, frame: WsMessage) -> Result<(), SendError> {
        match self.try_enqueue(frame) {
            Ok(_) => Ok(()),
            Err(SendError::BackpressureDrop) => {
                metrics::counter!("ws.send.error.lifecycle").increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    reason = "backpressure",
                    "lifecycle ws send failed; turn will abort"
                );
                Err(SendError::LifecycleFailure(
                    "writer channel full for lifecycle frame".into(),
                ))
            }
            Err(SendError::Closed) => {
                metrics::counter!("ws.send.error.lifecycle").increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    reason = "closed",
                    "lifecycle ws send failed; turn will abort"
                );
                Err(SendError::LifecycleFailure(
                    "writer channel closed for lifecycle frame".into(),
                ))
            }
            Err(other) => Err(other),
        }
    }

    /// Durable notification: tool/task/approval. Errors are logged WARN; the
    /// ledger still records the event so a future replay catches up.
    fn send_durable(&self, frame: WsMessage, method: &str) -> Result<(), SendError> {
        match self.try_enqueue(frame) {
            Ok(_) => Ok(()),
            Err(SendError::BackpressureDrop) => {
                self.metrics.dropped_count.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("ws.send.drop.backpressure", "method" => method.to_string())
                    .increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    method,
                    reason = "backpressure",
                    "durable ws send dropped; emitting replay_lossy"
                );
                Err(SendError::BackpressureDrop)
            }
            Err(SendError::Closed) => {
                metrics::counter!("ws.send.drop.closed", "method" => method.to_string())
                    .increment(1);
                metrics::counter!("ws.send.error.durable", "method" => method.to_string())
                    .increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    method,
                    reason = "closed",
                    "durable ws send failed; client gone"
                );
                Err(SendError::Closed)
            }
            Err(other) => Err(other),
        }
    }

    /// Ephemeral frame: `message/delta`. Drops are silent per spec § 9.
    fn send_ephemeral(&self, frame: WsMessage, method: &str) -> Result<(), SendError> {
        match self.try_enqueue(frame) {
            Ok(_) => Ok(()),
            Err(SendError::BackpressureDrop) => {
                tracing::debug!(
                    target: "octos::ui_protocol::ws",
                    method,
                    "ephemeral ws send dropped under backpressure"
                );
                Err(SendError::BackpressureDrop)
            }
            Err(SendError::Closed) => {
                metrics::counter!("ws.send.drop.closed", "method" => method.to_string())
                    .increment(1);
                tracing::debug!(
                    target: "octos::ui_protocol::ws",
                    method,
                    "ephemeral ws send dropped; channel closed"
                );
                Err(SendError::Closed)
            }
            Err(other) => Err(other),
        }
    }

    /// Dedicated writer-task loop: drains the channel into the actual sink.
    ///
    /// Exits on the first sink error (peer gone) or once all senders drop.
    /// We deliberately do not hold a lock across `sink.send().await` — the
    /// channel is the lock-free coordination point.
    pub(crate) async fn writer_loop(mut sink: WsSink, mut rx: mpsc::Receiver<WsMessage>) {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        // Best-effort close — ignore errors; peer may already be gone.
        let _ = sink.close().await;
    }
}

#[derive(Default)]
struct UiProtocolContractStores {
    approvals: PendingApprovalStore,
    /// Lazily-initialized pending diff-preview store. With a `data_dir`
    /// the first call hydrates from disk and subsequent inserts
    /// write-ahead before returning, so `diff/preview/get` survives
    /// daemon restart (mirrors the M9.6 ledger durability pattern).
    /// Without a `data_dir` (unit tests, headless smoke) we fall back
    /// to an ephemeral RAM-only store via `Default`.
    diff_previews: OnceLock<Arc<PendingDiffPreviewStore>>,
    /// Per-session approval-scope policy table — stores future-call gating
    /// rules registered by `respond` when the user picks a scope stronger
    /// than `approve_once`. See `ui_protocol_scope.rs`.
    scopes: ScopePolicy,
    /// Lazily-initialized append-only audit log for approval decisions
    /// (FIX-07). The first decision creates the log under
    /// `<data_dir>/audit/approvals-<epoch>.log`; subsequent decisions reuse
    /// the same writer.
    audit: OnceLock<Arc<ApprovalsAuditLog>>,
}

impl UiProtocolContractStores {
    fn audit_log(&self, data_dir: &Path) -> Arc<ApprovalsAuditLog> {
        self.audit
            .get_or_init(|| {
                Arc::new(ApprovalsAuditLog::new(
                    data_dir,
                    ApprovalsAuditConfig::from_env(),
                ))
            })
            .clone()
    }

    /// Lazily build the durable diff-preview store. The first caller
    /// with a `data_dir` wins and runs disk recovery; without a
    /// `data_dir` we install an ephemeral store. Subsequent calls
    /// always return the same `Arc`.
    fn diff_previews(&self, data_dir: Option<&Path>) -> Arc<PendingDiffPreviewStore> {
        self.diff_previews
            .get_or_init(|| {
                let config = match data_dir {
                    Some(dir) => DiffPreviewConfig::durable(dir.to_path_buf()),
                    None => DiffPreviewConfig::ephemeral(),
                };
                if config.data_dir.is_some() {
                    let outcome = PendingDiffPreviewStore::recover(config);
                    info!(
                        target = "octos::diff_preview",
                        sessions_recovered = outcome.sessions_recovered,
                        entries_recovered = outcome.entries_recovered,
                        "ui protocol diff-preview store initialized with durable backing"
                    );
                    Arc::new(outcome.store)
                } else {
                    Arc::new(PendingDiffPreviewStore::with_config(config))
                }
            })
            .clone()
    }
}

#[derive(Default)]
struct SessionWorkspaceStore {
    roots: std::sync::Mutex<HashMap<SessionKey, PathBuf>>,
}

impl SessionWorkspaceStore {
    fn set(&self, session_id: SessionKey, root: PathBuf) {
        self.roots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(session_id, root);
    }

    fn get(&self, session_id: &SessionKey) -> Option<PathBuf> {
        self.roots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(session_id)
            .cloned()
    }
}

/// Per-turn lifecycle state tracked by the registry under a single `Mutex`
/// guard. Together with the `interrupt_tx` signalling channel, this is the
/// boundary that makes interrupt-vs-natural-completion atomic and ensures
/// exactly one terminal event reaches the wire.
///
/// State transitions:
/// ```text
///        (turn/start)
///             |
///             v
///   +------- Active -------+
///   |          |           |
///   |   (handler           |   (task observes
///   |    interrupts)       |    natural finish)
///   |          v           |          v
///   |    Interrupting      |   Terminal(Completed)
///   |          |           |          /
///   |    (task acks)       |   Terminal(Errored)
///   |          v           v
///   +--> Terminal(Interrupted) <------+
/// ```
/// All terminal-event emission sites must lock the state, observe `Active` or
/// `Interrupting`, and atomically transition to `Terminal(_)` before sending.
/// Any path that sees a `Terminal(_)` state is a no-op (lost the race).
#[derive(Debug)]
enum TurnState {
    /// Turn is running normally; eligible for interrupt.
    Active,
    /// Handler captured an interrupt request and is waiting for the task to
    /// emit the terminal event and signal `ack`.
    Interrupting { ack: oneshot::Sender<()> },
    /// Terminal state — exactly one terminal event has been emitted.
    Terminal(TerminalReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalReason {
    Completed,
    Errored,
    Interrupted,
}

impl TerminalReason {
    fn as_str(self) -> &'static str {
        match self {
            TerminalReason::Completed => "completed",
            TerminalReason::Errored => "errored",
            TerminalReason::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum M9ProtocolFixture {
    Basic,
    Slow,
    ToolEvents,
    Approval,
    ReplayLossy,
    TaskOutput,
}

fn m9_protocol_fixture_for_prompt(prompt: &str) -> Option<M9ProtocolFixture> {
    if std::env::var("OCTOS_M9_PROTOCOL_FIXTURES").as_deref() != Ok("1") {
        return None;
    }

    let prompt = prompt.to_ascii_lowercase();
    if prompt.contains("m9 approval fixture") || prompt.contains("m9-approval-e2e") {
        Some(M9ProtocolFixture::Approval)
    } else if prompt.contains("m9 replay-lossy fixture") || prompt.contains("replay-lossy") {
        Some(M9ProtocolFixture::ReplayLossy)
    } else if prompt.contains("m9 task output fixture") {
        Some(M9ProtocolFixture::TaskOutput)
    } else if prompt.contains("list_dir tool") {
        Some(M9ProtocolFixture::ToolEvents)
    } else if prompt.contains("200 separate lines") || prompt.contains("one line at a time") {
        Some(M9ProtocolFixture::Slow)
    } else {
        Some(M9ProtocolFixture::Basic)
    }
}

struct ActiveTurn {
    turn_id: TurnId,
    /// Per-turn state guard; held by both the registry entry and by the turn
    /// task so interrupt + natural-completion races serialize on a single lock.
    state: Arc<TokioMutex<TurnState>>,
    /// Single-shot wake-up so the turn loop can return from `progress_rx.recv`
    /// promptly when an interrupt arrives. `None` once consumed.
    interrupt_tx: Arc<TokioMutex<Option<mpsc::Sender<()>>>>,
    abort: AbortHandle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ConnectionUiFeatures {
    typed_approvals: bool,
    pane_snapshots: bool,
    session_workspace_cwd: bool,
}

impl ConnectionUiFeatures {
    fn from_headers_and_query(headers: &HeaderMap, query: Option<&str>) -> Self {
        Self {
            typed_approvals: has_ui_feature(headers, query, UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1),
            pane_snapshots: has_ui_feature(headers, query, UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1),
            session_workspace_cwd: has_ui_feature(
                headers,
                query,
                UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
            ),
        }
    }
}

fn has_ui_feature(headers: &HeaderMap, query: Option<&str>, feature: &str) -> bool {
    headers
        .get(UI_FEATURES_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split([',', ' '])
                .any(|candidate| candidate.trim() == feature)
        })
        || query
            .unwrap_or_default()
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .filter(|(key, _)| matches!(*key, "ui_feature" | "ui_features" | "x-octos-ui-features"))
            .flat_map(|(_, value)| value.split([',', ' ']))
            .any(|candidate| candidate.trim() == feature)
}

#[derive(Default)]
struct TaskOutputDeltaTracker {
    active_task_id: Option<TaskId>,
    offsets: HashMap<TaskId, u64>,
}

impl TaskOutputDeltaTracker {
    fn observe_progress_event(
        &mut self,
        session_id: &SessionKey,
        event: &Value,
    ) -> Option<TaskOutputDeltaEvent> {
        let event_type = event.get("type").and_then(Value::as_str);
        if event_type == Some("task_started") {
            self.active_task_id = task_id_field(event);
        }

        let task_id = task_id_field(event).or_else(|| self.active_task_id.clone())?;
        let text = task_output_delta_text(event)?;
        let offset = self.offsets.entry(task_id.clone()).or_insert(0);
        let start_offset = *offset;
        let cursor = OutputCursor {
            offset: start_offset,
        };
        *offset = start_offset.saturating_add(text.len() as u64);

        Some(TaskOutputDeltaEvent {
            session_id: session_id.clone(),
            task_id,
            cursor,
            text,
        })
    }
}

fn active_turns_registry() -> SharedActiveTurns {
    static ACTIVE_TURNS: OnceLock<SharedActiveTurns> = OnceLock::new();
    ACTIVE_TURNS
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(HashMap::new())))
        .clone()
}

fn contract_stores() -> Arc<UiProtocolContractStores> {
    static CONTRACT_STORES: OnceLock<Arc<UiProtocolContractStores>> = OnceLock::new();
    CONTRACT_STORES
        .get_or_init(|| Arc::new(UiProtocolContractStores::default()))
        .clone()
}

fn session_workspaces() -> Arc<SessionWorkspaceStore> {
    static SESSION_WORKSPACES: OnceLock<Arc<SessionWorkspaceStore>> = OnceLock::new();
    SESSION_WORKSPACES
        .get_or_init(|| Arc::new(SessionWorkspaceStore::default()))
        .clone()
}

/// Process-global event ledger.
///
/// First call decides the durability path:
/// - With a `data_dir` from `AppState.sessions`, builds a Path-A durable
///   ledger, runs disk recovery, and spawns the idle-eviction sweep.
/// - Without a sessions manager (unit tests, headless smoke), builds a
///   RAM-only ledger that still enforces the LRU + idle-TTL caps but
///   does not persist.
///
/// Subsequent calls return the same `Arc`, regardless of what the new
/// caller passes — by design, the ledger is process-singleton.
async fn event_ledger(state: &AppState) -> Arc<UiProtocolLedger> {
    static EVENT_LEDGER: OnceLock<Arc<UiProtocolLedger>> = OnceLock::new();
    if let Some(existing) = EVENT_LEDGER.get() {
        return existing.clone();
    }
    let data_dir = match &state.sessions {
        Some(sessions) => Some(sessions.lock().await.data_dir()),
        None => None,
    };
    let config = match data_dir {
        Some(dir) => LedgerConfig::durable(dir),
        None => LedgerConfig::ephemeral(EVENT_LEDGER_RETAINED_PER_SESSION),
    };
    let ledger = if config.data_dir.is_some() {
        let outcome = UiProtocolLedger::recover(config);
        info!(
            target = "octos::ledger",
            sessions_recovered = outcome.sessions_recovered,
            events_recovered = outcome.events_recovered,
            "ui protocol ledger initialized with durable backing"
        );
        outcome.ledger
    } else {
        Arc::new(UiProtocolLedger::with_config(config))
    };
    let installed = EVENT_LEDGER.get_or_init(|| ledger.clone()).clone();
    // Only spawn the sweep task on the install path. If two connections
    // race here, only one wins the get_or_init and only that one starts
    // the sweep.
    if Arc::ptr_eq(&installed, &ledger) {
        let _handle = spawn_eviction_task(installed.clone());
    }
    installed
}

/// Process-global pending diff-preview store. Mirrors
/// [`event_ledger`]'s lazy initialization: with a `data_dir` from the
/// sessions manager, the first call hydrates from disk and installs a
/// durable store; without one we install an ephemeral fallback.
/// Subsequent calls return the same `Arc` regardless of the
/// `state` they're given — by design, the store is process-singleton.
async fn diff_preview_store(
    state: &AppState,
    contracts: &UiProtocolContractStores,
) -> Arc<PendingDiffPreviewStore> {
    let data_dir = match &state.sessions {
        Some(sessions) => Some(sessions.lock().await.data_dir()),
        None => None,
    };
    contracts.diff_previews(data_dir.as_deref())
}

struct AbortOnDrop {
    abort: AbortHandle,
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

struct BoundedChannelReporter {
    tx: tokio::sync::mpsc::Sender<String>,
    /// Mirrors WS-layer drops: when the progress channel is full the agent
    /// produced an event the WS layer will never see. Without this counter
    /// the cursor would lie. Surfaced opportunistically as `protocol/replay_lossy`
    /// from the consuming task.
    progress_dropped: Arc<AtomicU64>,
}

impl BoundedChannelReporter {
    fn new(tx: tokio::sync::mpsc::Sender<String>, progress_dropped: Arc<AtomicU64>) -> Self {
        Self {
            tx,
            progress_dropped,
        }
    }
}

impl octos_agent::ProgressReporter for BoundedChannelReporter {
    fn report(&self, event: octos_agent::ProgressEvent) {
        let json = match serde_json::to_string(&super::sse::event_to_json(&event, None)) {
            Ok(json) => json,
            Err(_) => return,
        };
        if let Err(err) = self.tx.try_send(json) {
            self.progress_dropped.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("ws.send.drop.backpressure", "method" => "progress").increment(1);
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                reason = ?err,
                "progress event dropped before reaching ws layer"
            );
        }
    }
}

/// Forward a `BackgroundTask` snapshot from `TaskSupervisor::set_on_change`
/// into the per-turn progress channel.
///
/// **Terminal updates** (`completed` / `failed` / `cancelled`) MUST NOT be
/// dropped under WebSocket backpressure — dropping one leaves the UI
/// stuck on `running` indefinitely even though the agent has long since
/// moved on (M9 review finding #6). For these, a `try_send` failure
/// upgrades to a spawned `tx.send().await` with a [`TERMINAL_TASK_SEND_TIMEOUT`]
/// budget so the update is durable through ordinary backpressure but does
/// not pile up zombies if the consumer is permanently gone.
///
/// **Non-terminal updates** are coalesce-friendly: the next update will
/// overwrite, so a drop has no correctness impact and we keep the
/// non-blocking `try_send` fast-path.
///
/// `progress_dropped` increments on the immediate `try_send` failure (so
/// the `protocol/replay_lossy` machinery is informed), regardless of
/// terminal status. The dedicated `ws.send.timeout.terminal` metric fires
/// only when even the awaited send hits the timeout — i.e., the case the
/// fix exists to make observable.
fn forward_task_progress_to_channel(
    tx: &tokio::sync::mpsc::Sender<String>,
    progress_dropped: &Arc<AtomicU64>,
    task: &octos_agent::BackgroundTask,
) {
    let event = background_task_to_progress_json(task);
    let Ok(json) = serde_json::to_string(&event) else {
        return;
    };
    if tx.try_send(json.clone()).is_ok() {
        return;
    }
    progress_dropped.fetch_add(1, Ordering::Relaxed);
    metrics::counter!("ws.send.drop.backpressure", "method" => "task_progress").increment(1);
    if !task.status.is_terminal() {
        // Non-terminal: drop is fine, next update overwrites.
        return;
    }
    // Terminal: spawn a durable awaited send. The runtime owns the JoinHandle,
    // so this survives the sync callback returning. A `tx.send().await` failure
    // means the receiver was dropped (turn over) — nothing to deliver to. The
    // timeout protects against a permanently-stuck consumer.
    let tx = tx.clone();
    let task_id = task.id.clone();
    let lifecycle = task.lifecycle_state();
    tokio::spawn(async move {
        match tokio::time::timeout(TERMINAL_TASK_SEND_TIMEOUT, tx.send(json)).await {
            Ok(Ok(())) => {}
            Ok(Err(_send_err)) => {
                // Receiver dropped; nothing observable to deliver. Not a bug.
                tracing::debug!(
                    target: "octos::ui_protocol::ws",
                    %task_id,
                    ?lifecycle,
                    "terminal task update dropped: progress receiver gone"
                );
            }
            Err(_elapsed) => {
                metrics::counter!(
                    "ws.send.timeout.terminal",
                    "method" => "task_progress"
                )
                .increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    %task_id,
                    ?lifecycle,
                    timeout_ms = TERMINAL_TASK_SEND_TIMEOUT.as_millis() as u64,
                    "terminal task update timed out under sustained backpressure"
                );
            }
        }
    });
}

struct UiProtocolApprovalRequester {
    ws: WsConnection,
    ledger: Arc<UiProtocolLedger>,
    contracts: Arc<UiProtocolContractStores>,
    /// Held so the FIX-07 audit log can resolve `<data_dir>/audit/` from
    /// `state.sessions.lock().data_dir()` on the auto-resolved decision
    /// path (and any future direct-decision paths).
    state: Arc<AppState>,
    session_id: SessionKey,
    turn_id: TurnId,
    features: ConnectionUiFeatures,
}

#[async_trait::async_trait]
impl octos_agent::ToolApprovalRequester for UiProtocolApprovalRequester {
    async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        let approval_id = ApprovalId::new();
        let event = approval_event_from_tool_request(
            request,
            self.session_id.clone(),
            approval_id.clone(),
            self.turn_id.clone(),
            self.features,
        );

        // Scope-policy short circuit: if the user previously chose
        // `approve_for_*` for a matching tool/turn/session, resolve this
        // approval automatically. Emit BOTH:
        //   1. `approval/auto_resolved` (FIX-06): informational, carries
        //      the scope/match identifiers so the client can reason about
        //      *why* the request did not surface.
        //   2. `approval/decided` (FIX-07): the canonical durable record
        //      of the decision; flagged with `auto_resolved = true` and
        //      a `policy_id` so audit/replay treat it identically to a
        //      manual decision.
        // The audit log writer also runs here so auto-resolved decisions
        // appear in the JSON-Lines log next to manual ones (compliance
        // requirement: every decision is recorded).
        if let Some(hit) =
            self.contracts
                .scopes
                .lookup(&self.session_id, &event.tool_name, &self.turn_id)
        {
            // FIX-01: `ApprovalDecision` is non-Copy because of `Unknown(String)`;
            // clone for the wire payload so the original survives for the
            // runtime decision below.
            let auto = ApprovalAutoResolvedEvent {
                session_id: self.session_id.clone(),
                approval_id: approval_id.clone(),
                turn_id: self.turn_id.clone(),
                tool_name: event.tool_name.clone(),
                scope: hit.scope_wire().to_owned(),
                scope_match: hit.scope_match.clone(),
                decision: hit.decision.clone(),
            };
            // Best-effort: if the notification fails to send (connection
            // closed) we still apply the recorded decision — the runtime
            // already trusts the policy. Per FIX-04, `approval/auto_resolved`
            // is durable: drops surface as `protocol/replay_lossy`.
            let _ = send_notification_durable(
                &self.ws,
                &self.ledger,
                UiNotification::ApprovalAutoResolved(auto),
            );

            // FIX-07: build + emit the canonical `approval/decided` record.
            // `decided_by` is empty because the decision is system-issued
            // (matches the spec's "system-decided" convention).
            let policy_id = format!("policy:{}:{}", hit.scope_wire(), hit.scope_match);
            let decided_event = ApprovalDecidedEvent {
                session_id: self.session_id.clone(),
                approval_id: approval_id.clone(),
                turn_id: self.turn_id.clone(),
                decision: hit.decision.clone(),
                scope: Some(hit.scope_wire().to_owned()),
                decided_at: Utc::now(),
                decided_by: String::new(),
                auto_resolved: true,
                policy_id: Some(policy_id),
                client_note: None,
            };
            log_decision_tracing(&decided_event, Some(event.tool_name.as_str()));
            if let Some(sessions) = self.state.sessions.as_ref() {
                let data_dir = sessions.lock().await.data_dir();
                let audit = self.contracts.audit_log(&data_dir);
                if let Err(error) = audit.record(&decided_event, Some(event.tool_name.as_str())) {
                    tracing::warn!(
                        target: "octos.approvals.decision",
                        approval_id = %decided_event.approval_id.0,
                        error = %error,
                        "failed to append approval audit log entry (auto-resolved)"
                    );
                }
            }
            let _ = send_notification_durable(
                &self.ws,
                &self.ledger,
                UiNotification::ApprovalDecided(decided_event),
            );

            return match hit.decision {
                ApprovalDecision::Approve => ToolApprovalDecision::Approve,
                ApprovalDecision::Deny => ToolApprovalDecision::Deny,
                // FIX-01: forward-compat fallback. A recorded decision the
                // current server doesn't understand fails closed.
                ApprovalDecision::Unknown(_) => ToolApprovalDecision::Deny,
            };
        }

        let response_rx = self.contracts.approvals.request_runtime(event.clone());
        // Approvals are durable: if the WS drop strands the request, the
        // ledger still records it and the client can rehydrate; we still
        // deny here to avoid tools running without confirmation.
        if let Err(err) = send_notification_durable(
            &self.ws,
            &self.ledger,
            UiNotification::ApprovalRequested(event),
        ) {
            cancel_approval_after_request_send_failure(
                self.contracts.as_ref(),
                &self.ws,
                &self.ledger,
                &self.session_id,
                &approval_id,
                &self.turn_id,
            );
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                error = ?err,
                "approval/requested notification not delivered; denying"
            );
            return ToolApprovalDecision::Deny;
        }

        match response_rx.await.unwrap_or(ApprovalDecision::Deny) {
            ApprovalDecision::Approve => ToolApprovalDecision::Approve,
            ApprovalDecision::Deny => ToolApprovalDecision::Deny,
            // FIX-01 added Unknown(_) for forward-compat. Treat any
            // unrecognized decision as Deny — fail closed at the trust
            // boundary.
            ApprovalDecision::Unknown(_) => ToolApprovalDecision::Deny,
        }
    }
}

fn cancel_approval_after_request_send_failure(
    contracts: &UiProtocolContractStores,
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    approval_id: &ApprovalId,
    turn_id: &TurnId,
) {
    let Some(cancelled) = contracts.approvals.cancel_pending_approval(
        session_id,
        approval_id,
        turn_id,
        APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED,
    ) else {
        return;
    };

    let _ = send_notification_durable(
        ws,
        ledger,
        UiNotification::ApprovalCancelled(ApprovalCancelledEvent {
            session_id: session_id.clone(),
            approval_id: cancelled.approval_id,
            turn_id: cancelled.turn_id,
            reason: APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED.to_owned(),
        }),
    );
}

fn approval_event_from_tool_request(
    request: ToolApprovalRequest,
    session_id: SessionKey,
    approval_id: ApprovalId,
    turn_id: TurnId,
    features: ConnectionUiFeatures,
) -> ApprovalRequestedEvent {
    let mut event = ApprovalRequestedEvent::generic(
        session_id,
        approval_id,
        turn_id,
        request.tool_name,
        request.title,
        request.body,
    );

    if features.typed_approvals && event.tool_name == "shell" {
        let command = request.command;
        if command.is_some() || request.cwd.is_some() {
            event.approval_kind = Some(approval_kinds::COMMAND.to_owned());
            // Risk is derived from the tool manifest, not from the tool's
            // own payload — a malicious tool cannot self-attest as `low`.
            // Default `unspecified` makes "manifest didn't say" visible in
            // the UI badge instead of silently advertising `medium`.
            event.risk = Some(server_risk_for(&event.tool_name));
            // `cwd` is path-shaped: sanitise before it lands in display
            // strings (typed_details, render hints).
            let safe_cwd = request.cwd.as_deref().map(sanitize_display_path);
            event.typed_details = Some(ApprovalTypedDetails::command(
                ApprovalCommandDetails {
                    argv: Vec::new(),
                    command_line: command,
                    cwd: safe_cwd,
                    env_keys: Vec::new(),
                    tool_call_id: Some(request.tool_id),
                },
                None,
            ));
            event.render_hints = Some(ApprovalRenderHints {
                default_decision: Some("deny".to_owned()),
                primary_label: Some("Approve".to_owned()),
                secondary_label: Some("Deny".to_owned()),
                danger: Some(false),
                monospace_fields: vec![
                    "typed_details.command.command_line".to_owned(),
                    "typed_details.command.cwd".to_owned(),
                ],
            });
        }
    }

    event
}

/// Resolve the manifest-declared risk for `tool_name`. Falls back to
/// `unspecified` when the registry has no entry.
fn server_risk_for(tool_name: &str) -> String {
    octos_core::ui_protocol::tool_approval_risk(tool_name)
}

#[cfg(test)]
fn register_tool_risk_for_test(tool_name: &str, risk: &str) {
    octos_core::ui_protocol::register_tool_approval_risk(tool_name, risk);
}

#[cfg(test)]
fn clear_tool_risk_registry_for_test() {
    octos_core::ui_protocol::clear_tool_approval_risks_for_test();
}

#[cfg(test)]
fn tool_risk_registry_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// GET /api/ui-protocol/ws — JSON-RPC over WebSocket for UI Protocol v1.
pub async fn ws_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<AuthIdentity>>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Response {
    let connection_profile_id = identity
        .as_ref()
        .and_then(|Extension(identity)| authenticated_profile_id(identity))
        .map(ToOwned::to_owned);
    let features = ConnectionUiFeatures::from_headers_and_query(&headers, uri.query());
    ws.on_upgrade(move |socket| {
        ui_protocol_connection(socket, state, connection_profile_id, features)
    })
}

async fn ui_protocol_connection(
    socket: WebSocket,
    state: Arc<AppState>,
    connection_profile_id: Option<String>,
    features: ConnectionUiFeatures,
) {
    let (ws_sink, mut ws_rx) = socket.split();
    // Decouple the network sink from request handlers via a bounded channel
    // and a dedicated drainer task. No handler ever holds a lock across an
    // await on the socket — that fixes the slow-client wedge.
    let (writer_tx, writer_rx) = mpsc::channel::<WsMessage>(WS_WRITER_CHANNEL_CAPACITY);
    let writer_handle = tokio::spawn(WsConnection::writer_loop(ws_sink, writer_rx));
    let ws = WsConnection::new(writer_tx);
    let active_turns = active_turns_registry();
    let connection_turns: SharedConnectionTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let contracts = contract_stores();
    let ledger = event_ledger(&state).await;
    // Force lazy init of the diff-preview store on this connection so
    // its disk recovery + write-ahead path is wired up before any
    // approval flow can `upsert_file_mutation`. Subsequent calls reuse
    // the same `Arc`. Without `state.sessions` (headless smoke) this
    // installs the ephemeral RAM-only fallback.
    let _ = diff_preview_store(&state, contracts.as_ref()).await;
    let connection_profile_id = connection_profile_id.as_deref();

    while let Some(Ok(msg)) = ws_rx.next().await {
        let text = match msg {
            WsMessage::Text(text) => text,
            WsMessage::Close(_) => break,
            WsMessage::Ping(_) => continue,
            _ => continue,
        };

        let request = match parse_ws_text_frame(text.as_str()) {
            Ok(request) => request,
            Err(error) => {
                // Lifecycle: client violated the wire contract. We try to
                // tell them, but proceed regardless — the read loop is
                // independent of the write side.
                let _ = send_rpc_error(&ws, None, error);
                if text.len() > MAX_TEXT_FRAME_BYTES {
                    break;
                }
                continue;
            }
        };
        let id = request.id.clone();
        let command = match route_rpc_command(request) {
            Ok(command) => command,
            Err(error) => {
                let _ = send_rpc_error(&ws, Some(id), error);
                continue;
            }
        };

        match command {
            UiCommand::SessionOpen(params) => {
                handle_session_open(
                    &ws,
                    &state,
                    &ledger,
                    &contracts.approvals,
                    connection_profile_id,
                    features,
                    id,
                    params,
                )
                .await;
            }
            UiCommand::TurnStart(params) => {
                handle_turn_start(
                    &ws,
                    &state,
                    &ledger,
                    &contracts,
                    &active_turns,
                    &connection_turns,
                    connection_profile_id,
                    features,
                    id,
                    params,
                )
                .await;
            }
            UiCommand::TurnInterrupt(params) => {
                handle_turn_interrupt(&ws, &ledger, &active_turns, &contracts, id, params).await;
            }
            UiCommand::ApprovalRespond(params) => {
                handle_approval_respond(
                    &ws,
                    &state,
                    &ledger,
                    &contracts,
                    connection_profile_id,
                    id,
                    params,
                )
                .await;
            }
            UiCommand::ApprovalScopesList(params) => {
                handle_approval_scopes_list(
                    &ws,
                    &contracts.scopes,
                    connection_profile_id,
                    id,
                    params,
                )
                .await;
            }
            UiCommand::DiffPreviewGet(params) => {
                let store = diff_preview_store(&state, contracts.as_ref()).await;
                handle_diff_preview_get(&ws, store.as_ref(), connection_profile_id, id, params)
                    .await;
            }
            UiCommand::TaskOutputRead(params) => {
                handle_task_output_read(&ws, &state, connection_profile_id, id, params).await;
            }
            UiCommand::TaskList(params) => {
                handle_task_list(&ws, &state, connection_profile_id, id, params).await;
            }
            UiCommand::TaskCancel(params) => {
                handle_task_cancel(&ws, &state, connection_profile_id, id, params).await;
            }
            UiCommand::TaskRestartFromNode(params) => {
                handle_task_restart_from_node(&ws, &state, connection_profile_id, id, params).await;
            }
        }
    }

    abort_connection_turns(&active_turns, &connection_turns, &contracts.scopes).await;
    // Dropping `ws` lets the writer task drain & exit; await it so the socket
    // is closed before we return.
    drop(ws);
    let _ = writer_handle.await;
}

fn parse_ws_text_frame(text: &str) -> Result<RpcRequest<Value>, RpcError> {
    if text.len() > MAX_TEXT_FRAME_BYTES {
        return Err(frame_too_large_error());
    }
    parse_rpc_request(text)
}

fn parse_rpc_request(text: &str) -> Result<RpcRequest<Value>, RpcError> {
    serde_json::from_str(text).map_err(|err| RpcError::parse_error(err.to_string()))
}

fn route_rpc_command(request: RpcRequest<Value>) -> Result<UiCommand, RpcError> {
    let command = UiCommand::from_rpc_request(request)?;
    if !ui_protocol_server_supported_methods().contains(&command.method()) {
        return Err(RpcError::method_not_supported(command.method()));
    }
    Ok(command)
}

fn ui_protocol_server_supported_methods() -> Vec<&'static str> {
    octos_core::ui_protocol::UI_PROTOCOL_FIRST_SERVER_METHODS.to_vec()
}

fn frame_too_large_error() -> RpcError {
    RpcError::new(
        FRAME_TOO_LARGE,
        format!("WebSocket text frame exceeds {MAX_TEXT_FRAME_BYTES} bytes"),
    )
    .with_data(json!({ "limit_bytes": MAX_TEXT_FRAME_BYTES }))
}

fn authenticated_profile_id(identity: &AuthIdentity) -> Option<&str> {
    match identity {
        AuthIdentity::User { id, .. } if !id.is_empty() => Some(id),
        AuthIdentity::User { .. } | AuthIdentity::Admin => None,
    }
}

fn validate_session_scope(
    session_id: &SessionKey,
    requested_profile_id: Option<&str>,
    connection_profile_id: Option<&str>,
) -> Result<Option<String>, RpcError> {
    if requested_profile_id.is_some_and(str::is_empty) {
        return Err(RpcError::invalid_params("profile_id cannot be empty"));
    }

    if let Some(connection_profile_id) = connection_profile_id {
        validate_authenticated_session_scope(
            session_id,
            requested_profile_id,
            connection_profile_id,
        )?;
        return Ok(Some(connection_profile_id.to_string()));
    }

    if let (Some(requested_profile_id), Some(session_profile_id)) =
        (requested_profile_id, session_id.profile_id())
    {
        if requested_profile_id != session_profile_id {
            return Err(profile_mismatch_error(
                "profile_id does not match session_id profile",
                session_profile_id,
                Some(requested_profile_id),
            ));
        }
    }

    Ok(requested_profile_id
        .or_else(|| session_id.profile_id())
        .map(ToOwned::to_owned))
}

fn validate_authenticated_session_scope(
    session_id: &SessionKey,
    requested_profile_id: Option<&str>,
    connection_profile_id: &str,
) -> Result<(), RpcError> {
    if requested_profile_id.is_some_and(|profile_id| profile_id != connection_profile_id) {
        return Err(profile_mismatch_error(
            "profile_id is outside the authenticated profile",
            connection_profile_id,
            requested_profile_id,
        ));
    }

    match session_id.profile_id() {
        Some(session_profile_id) if session_profile_id == connection_profile_id => Ok(()),
        Some(session_profile_id) => Err(profile_mismatch_error(
            "session_id is outside the authenticated profile",
            connection_profile_id,
            Some(session_profile_id),
        )),
        None => Err(
            RpcError::invalid_params("session_id must include the authenticated profile")
                .with_data(json!({
                    "expected_profile_id": connection_profile_id,
                })),
        ),
    }
}

fn profile_mismatch_error(
    message: &'static str,
    expected_profile_id: &str,
    actual_profile_id: Option<&str>,
) -> RpcError {
    RpcError::invalid_params(message).with_data(json!({
        "expected_profile_id": expected_profile_id,
        "actual_profile_id": actual_profile_id,
    }))
}

async fn handle_session_open(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &UiProtocolLedger,
    approvals: &PendingApprovalStore,
    connection_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: SessionOpenParams,
) {
    let outcome = match open_session_result(
        state,
        ledger,
        approvals,
        connection_profile_id,
        features,
        params,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };

    let result = match serde_json::to_value(outcome.result) {
        Ok(result) => result,
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!(
                    "failed to serialize session/open result: {error}"
                )),
            );
            return;
        }
    };
    // session/open reply is the lifecycle frame that the client blocks on;
    // if it fails the connection is doomed for this command.
    if send_rpc_result(ws, id, result).is_err() {
        return;
    }
    // Replay frames are durable: drops surface as `protocol/replay_lossy`
    // and the client can refetch via REST.
    for event in outcome.replay {
        let _ = send_ledger_event_durable(ws, ledger, event.event);
    }
    for event in outcome.pending_approvals {
        let _ = send_ledger_event_durable(
            ws,
            ledger,
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(event)),
        );
    }
    let _ = send_ledger_event_durable(ws, ledger, outcome.opened_event.event);
}

#[derive(Debug)]
struct SessionOpenOutcome {
    result: SessionOpenResult,
    replay: Vec<LedgeredUiProtocolEvent>,
    pending_approvals: Vec<ApprovalRequestedEvent>,
    opened_event: LedgeredUiProtocolEvent,
}

async fn open_session_result(
    state: &Arc<AppState>,
    ledger: &UiProtocolLedger,
    approvals: &PendingApprovalStore,
    connection_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    params: SessionOpenParams,
) -> Result<SessionOpenOutcome, RpcError> {
    let active_profile_id = validate_session_scope(
        &params.session_id,
        params.profile_id.as_deref(),
        connection_profile_id,
    )?;
    let requested_workspace = validate_requested_session_cwd(state, features, &params)?;
    if let Some(workspace_root) = requested_workspace {
        session_workspaces().set(params.session_id.clone(), workspace_root);
    }
    let replay = ledger.replay_after(&params.session_id, params.after.as_ref())?;
    let replayed_approval_ids = replay
        .iter()
        .filter_map(|event| match &event.event {
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(approval)) => {
                Some(approval.approval_id.clone())
            }
            _ => None,
        })
        .collect::<HashSet<_>>();
    let pending_approvals = approvals
        .pending_for_session(&params.session_id)
        .into_iter()
        .filter(|approval| !replayed_approval_ids.contains(&approval.approval_id))
        .collect::<Vec<_>>();

    let Some(sessions) = &state.sessions else {
        return Err(runtime_unavailable_error("Sessions not available"));
    };

    let data_dir = {
        let mut sessions = sessions.lock().await;
        sessions.get_or_create(&params.session_id).await;
        sessions.data_dir()
    };

    let workspace_root = session_workspace_root_for_state(state, &params.session_id);
    let panes = features
        .pane_snapshots
        .then(|| build_pane_snapshot(&data_dir, &params.session_id, workspace_root.as_deref()));
    let opened_event = ledger.append_notification(UiNotification::SessionOpened(SessionOpened {
        session_id: params.session_id,
        active_profile_id,
        workspace_root: workspace_root.map(|path| path.to_string_lossy().to_string()),
        cursor: None,
        panes,
    }));
    let UiProtocolLedgerEvent::Notification(UiNotification::SessionOpened(opened)) =
        opened_event.event.clone()
    else {
        unreachable!("session/open ledger append returns session/open notification");
    };
    Ok(SessionOpenOutcome {
        result: SessionOpenResult::new(opened),
        replay,
        pending_approvals,
        opened_event,
    })
}

fn validate_requested_session_cwd(
    state: &AppState,
    features: ConnectionUiFeatures,
    params: &SessionOpenParams,
) -> Result<Option<PathBuf>, RpcError> {
    let Some(cwd) = params
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
    else {
        return Ok(None);
    };

    if !features.session_workspace_cwd {
        return Err(RpcError::invalid_params(
            "session/open cwd requires feature session.workspace_cwd.v1",
        )
        .with_data(json!({
            "kind": "feature_required",
            "feature": UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
        })));
    }

    let workspace_root = canonical_existing_dir(cwd)?;
    validate_session_workspace_allowed(state, &workspace_root)?;
    Ok(Some(workspace_root))
}

fn canonical_existing_dir(path: &str) -> Result<PathBuf, RpcError> {
    let expanded = expand_home_path(path);
    let canonical = std::fs::canonicalize(&expanded).map_err(|error| {
        RpcError::invalid_params(format!("session/open cwd is not accessible: {path}")).with_data(
            json!({
                "kind": "cwd_not_accessible",
                "cwd": path,
                "error": error.to_string(),
            }),
        )
    })?;
    if !canonical.is_dir() {
        return Err(RpcError::invalid_params(format!(
            "session/open cwd is not a directory: {path}"
        ))
        .with_data(json!({
            "kind": "cwd_not_directory",
            "cwd": path,
        })));
    }
    Ok(canonical)
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path));
    }
    PathBuf::from(path)
}

fn validate_session_workspace_allowed(
    state: &AppState,
    workspace_root: &Path,
) -> Result<(), RpcError> {
    let Some(agent) = &state.agent else {
        return Err(RpcError::invalid_params(
            "session/open cwd requires a configured coding runtime",
        )
        .with_data(json!({
            "kind": "cwd_runtime_unavailable",
            "cwd": workspace_root.to_string_lossy(),
        })));
    };

    let tools = agent.tool_registry();
    session_filesystem_profile_for_workspace(tools.as_ref(), workspace_root)
}

fn session_filesystem_profile_for_workspace(
    tools: &octos_agent::ToolRegistry,
    workspace_root: &Path,
) -> Result<(), RpcError> {
    if let Some(root) = tools.workspace_root() {
        if path_is_under_root(workspace_root, root) {
            return Ok(());
        }
        return Err(RpcError::invalid_params(
            "session/open cwd is outside the server workspace root",
        )
        .with_data(json!({
            "kind": "cwd_outside_workspace_root",
            "cwd": workspace_root.to_string_lossy(),
            "workspace_root": root.to_string_lossy(),
        })));
    }

    Err(
        RpcError::invalid_params("session/open cwd cannot be authorized by this runtime")
            .with_data(json!({
                "kind": "cwd_authorization_unavailable",
                "cwd": workspace_root.to_string_lossy(),
            })),
    )
}

fn session_workspace_root_for_state(state: &AppState, session_id: &SessionKey) -> Option<PathBuf> {
    session_workspaces().get(session_id).or_else(|| {
        state
            .agent
            .as_ref()?
            .tool_registry()
            .workspace_root()
            .map(Path::to_path_buf)
    })
}

fn session_tool_registry(
    base_agent: &Agent,
    session_id: &SessionKey,
) -> Result<(Arc<octos_agent::ToolRegistry>, Option<PathBuf>), String> {
    let base_tools = base_agent.tool_registry();
    let Some(workspace_root) = session_workspaces()
        .get(session_id)
        .or_else(|| base_tools.workspace_root().map(Path::to_path_buf))
    else {
        return Ok((base_tools.clone(), None));
    };

    session_filesystem_profile_for_workspace(base_tools.as_ref(), &workspace_root)
        .map_err(|error| error.message)?;
    // M9 review fix (HIGH #1): inherit the effective sandbox config from
    // `base_agent` so per-session shell tools keep the running server's
    // sandbox policy (mode, network, read-allow paths, profile) instead of
    // silently falling back to `SandboxConfig::default()` which disables
    // network and overrides read paths — that fallback was the root cause
    // of "backend cannot install npm" reports on AppUi sessions.
    // Falls back to `SandboxConfig::default()` only when the agent was built
    // without recording its sandbox config (legacy/test paths).
    let sandbox_config = base_agent
        .sandbox_config()
        .unwrap_or_else(octos_agent::SandboxConfig::default);
    let sandbox = octos_agent::sandbox::create_sandbox(&sandbox_config);
    let rebound = base_tools.rebind_cwd(&workspace_root, sandbox);

    Ok((Arc::new(rebound), Some(workspace_root)))
}

fn session_system_prompt(base_agent: &Agent, workspace_root: Option<&Path>) -> String {
    let mut prompt = base_agent.system_prompt_snapshot();
    if let Some(workspace_root) = workspace_root {
        prompt.push_str("\n\nAppUi session workspace root: ");
        prompt.push_str(&workspace_root.to_string_lossy());
        prompt.push_str(
            "\nThe server approved this cwd for the current session. Resolve relative shell and file-tool paths against this workspace.",
        );
    }
    prompt
}

fn path_is_under_root(path: &Path, root: &Path) -> bool {
    let path = canonical_or_original(path);
    let root = canonical_or_original(root);
    path == root || path.starts_with(root)
}

fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

const MAX_PANE_WORKSPACE_ENTRIES: usize = 200;
const MAX_PANE_ARTIFACT_ITEMS: usize = 80;
const MAX_PANE_GIT_HISTORY: usize = 12;

fn build_pane_snapshot(
    data_dir: &Path,
    session_id: &SessionKey,
    workspace_root: Option<&Path>,
) -> UiPaneSnapshot {
    let workspace_dirs = ui_protocol_session_workspace_dirs(data_dir, session_id, workspace_root);
    let mut limitations = Vec::new();
    let workspace = build_workspace_pane_snapshot(&workspace_dirs, &mut limitations);
    let artifacts = build_artifact_pane_snapshot(&workspace_dirs);
    let git = build_git_pane_snapshot(&workspace_dirs);

    UiPaneSnapshot {
        session_id: session_id.clone(),
        generated_at: Some(Utc::now()),
        workspace: Some(workspace),
        artifacts: Some(artifacts),
        git: Some(git),
        limitations,
    }
}

fn build_workspace_pane_snapshot(
    workspace_dirs: &[PathBuf],
    limitations: &mut Vec<UiPaneSnapshotLimitation>,
) -> UiWorkspacePaneSnapshot {
    let root = workspace_dirs
        .iter()
        .find(|path| path.exists())
        .or_else(|| workspace_dirs.first())
        .cloned()
        .unwrap_or_default();

    let mut entries = Vec::new();
    let mut truncated = false;
    if root.exists() {
        collect_workspace_entries(&root, &root, &mut entries, &mut truncated);
    } else {
        limitations.push(UiPaneSnapshotLimitation {
            code: "workspace_missing".into(),
            message: format!("workspace root does not exist: {}", root.display()),
        });
    }

    let mut workspace_limitations = Vec::new();
    if truncated {
        workspace_limitations.push(UiPaneSnapshotLimitation {
            code: "workspace_truncated".into(),
            message: format!("workspace tree limited to {MAX_PANE_WORKSPACE_ENTRIES} entries"),
        });
    }

    let root = root.to_string_lossy().to_string();
    UiWorkspacePaneSnapshot {
        root: root.clone(),
        readable_roots: vec![root.clone()],
        writable_roots: vec![root],
        contract: vec![
            "api octos-app-ui/v1alpha1".into(),
            "source session/open panes".into(),
            "feature pane.snapshots.v1".into(),
        ],
        entries,
        limitations: workspace_limitations,
    }
}

fn collect_workspace_entries(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<UiWorkspacePaneEntry>,
    truncated: &mut bool,
) {
    if entries.len() >= MAX_PANE_WORKSPACE_ENTRIES {
        *truncated = true;
        return;
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children = read_dir.flatten().collect::<Vec<_>>();
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        if entries.len() >= MAX_PANE_WORKSPACE_ENTRIES {
            *truncated = true;
            return;
        }

        let path = child.path();
        let file_name = child.file_name();
        let label = file_name.to_string_lossy().to_string();
        if should_skip_pane_dir(&label) {
            continue;
        }

        let Ok(metadata) = child.metadata() else {
            continue;
        };
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let relative_path = relative.to_string_lossy().to_string();
        let depth = relative.components().count().saturating_sub(1);
        let (kind, detail) = if metadata.is_dir() {
            ("directory", Some("dir".into()))
        } else if metadata.is_file() {
            ("file", Some(format_size(metadata.len())))
        } else if metadata.file_type().is_symlink() {
            ("symlink", None)
        } else {
            ("other", None)
        };

        entries.push(UiWorkspacePaneEntry {
            path: relative_path,
            label,
            depth,
            kind: kind.into(),
            detail,
        });

        if metadata.is_dir() {
            collect_workspace_entries(root, &path, entries, truncated);
        }
    }
}

fn build_artifact_pane_snapshot(workspace_dirs: &[PathBuf]) -> UiArtifactPaneSnapshot {
    let mut artifacts = Vec::new();
    for root in workspace_dirs.iter().filter(|path| path.exists()) {
        collect_artifact_items(root, root, &mut artifacts);
        if artifacts.len() >= MAX_PANE_ARTIFACT_ITEMS {
            break;
        }
    }

    artifacts.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.title.cmp(&right.1.title))
    });
    artifacts.truncate(MAX_PANE_ARTIFACT_ITEMS);

    let items = artifacts.into_iter().map(|(_, item)| item).collect();
    UiArtifactPaneSnapshot {
        items,
        limitations: Vec::new(),
    }
}

fn collect_artifact_items(
    root: &Path,
    dir: &Path,
    artifacts: &mut Vec<(std::time::SystemTime, UiArtifactPaneItem)>,
) {
    if artifacts.len() >= MAX_PANE_ARTIFACT_ITEMS {
        return;
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for child in read_dir.flatten() {
        if artifacts.len() >= MAX_PANE_ARTIFACT_ITEMS {
            return;
        }

        let path = child.path();
        let label = child.file_name().to_string_lossy().to_string();
        if should_skip_pane_dir(&label) {
            continue;
        }

        let Ok(metadata) = child.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_artifact_items(root, &path, artifacts);
            continue;
        }
        if !metadata.is_file() {
            continue;
        }

        let modified = metadata
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let updated_at = Some(chrono::DateTime::<Utc>::from(modified));
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let relative_path = relative.to_string_lossy().to_string();
        artifacts.push((
            modified,
            UiArtifactPaneItem {
                title: label,
                kind: "file".into(),
                path: Some(relative_path.clone()),
                uri: Some(relative_path),
                source: Some("workspace".into()),
                status: format_size(metadata.len()),
                source_task_id: None,
                preview_id: None,
                size_bytes: Some(metadata.len()),
                updated_at,
            },
        ));
    }
}

fn build_git_pane_snapshot(workspace_dirs: &[PathBuf]) -> UiGitPaneSnapshot {
    let Some(repo_root) = workspace_dirs
        .iter()
        .filter(|path| path.exists())
        .find_map(git_repo_root)
    else {
        return UiGitPaneSnapshot {
            repo_root: None,
            branch: None,
            head: None,
            clean: true,
            status: Vec::new(),
            history: Vec::new(),
            limitations: vec![UiPaneSnapshotLimitation {
                code: "git_unavailable".into(),
                message: "no git repository found for session workspace".into(),
            }],
        };
    };

    let branch = git_output(&repo_root, ["branch", "--show-current"]);
    let head = git_output(&repo_root, ["rev-parse", "--short", "HEAD"]);
    let status_output = git_output(&repo_root, ["status", "--porcelain=v1"]).unwrap_or_default();
    let status = status_output
        .lines()
        .filter_map(parse_git_status_line)
        .collect::<Vec<_>>();
    let history_limit = MAX_PANE_GIT_HISTORY.to_string();
    let history_output = git_output(
        &repo_root,
        ["log", "--oneline", "-n", history_limit.as_str()],
    )
    .unwrap_or_default();
    let history = history_output
        .lines()
        .filter_map(parse_git_history_line)
        .collect::<Vec<_>>();

    UiGitPaneSnapshot {
        repo_root: Some(repo_root.to_string_lossy().to_string()),
        branch,
        head,
        clean: status.is_empty(),
        status,
        history,
        limitations: Vec::new(),
    }
}

fn git_repo_root(path: &PathBuf) -> Option<PathBuf> {
    git_output(path, ["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

fn git_output<const N: usize>(repo_root: &Path, args: [&str; N]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn parse_git_status_line(line: &str) -> Option<UiGitStatusItem> {
    let code = line.get(0..2)?.trim().to_string();
    let path = line.get(3..)?.trim().to_string();
    if path.is_empty() {
        return None;
    }

    Some(UiGitStatusItem {
        detail: git_status_detail(&code).into(),
        code: if code.is_empty() { "?".into() } else { code },
        path,
    })
}

fn git_status_detail(code: &str) -> &'static str {
    match code {
        "M" | "MM" | "AM" | "A M" | " M" | "M " => "modified",
        "A" | "A " => "added",
        "D" | " D" | "D " => "deleted",
        "R" | "R " => "renamed",
        "??" => "untracked",
        _ => "changed",
    }
}

fn parse_git_history_line(line: &str) -> Option<UiGitHistoryItem> {
    let (commit, summary) = line.split_once(' ')?;
    Some(UiGitHistoryItem {
        commit: commit.into(),
        summary: summary.into(),
    })
}

fn should_skip_pane_dir(label: &str) -> bool {
    matches!(label, ".git" | "target" | "node_modules")
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn ui_protocol_session_workspace_dirs(
    data_dir: &Path,
    session_id: &SessionKey,
    workspace_root: Option<&Path>,
) -> Vec<PathBuf> {
    let profile_id = infer_profile_id_from_data_dir(data_dir);
    let mut dirs = Vec::with_capacity(4);
    let mut seen = HashSet::new();

    if let Some(workspace_root) = workspace_root {
        let path = workspace_root.to_path_buf();
        if seen.insert(path.clone()) {
            dirs.push(path);
        }
    }

    for key in [
        session_id.clone(),
        SessionKey::with_profile(&profile_id, session_id.channel(), session_id.chat_id()),
        SessionKey::with_profile(MAIN_PROFILE_ID, session_id.channel(), session_id.chat_id()),
        SessionKey::new(session_id.channel(), session_id.chat_id()),
    ] {
        let encoded_base = octos_bus::session::encode_path_component(key.base_key());
        let path = data_dir.join("users").join(encoded_base).join("workspace");
        if seen.insert(path.clone()) {
            dirs.push(path);
        }
    }

    dirs
}

fn infer_profile_id_from_data_dir(data_dir: &Path) -> String {
    data_dir
        .file_name()
        .and_then(|name| (name == "data").then_some(data_dir))
        .and_then(|_| data_dir.parent())
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(MAIN_PROFILE_ID)
        .to_string()
}

async fn handle_turn_start(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    active_turns: &SharedActiveTurns,
    connection_turns: &SharedConnectionTurns,
    connection_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: TurnStartParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }

    let Some(prompt) = prompt_text(&params.input) else {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params("turn/start requires at least one text input item"),
        );
        return;
    };

    let fixture = m9_protocol_fixture_for_prompt(&prompt);
    if fixture.is_none() {
        if let Err(error) = validate_runtime(state) {
            let _ = send_rpc_error(ws, Some(id), runtime_unavailable_error(error));
            return;
        }
    }

    let ws_for_turn = ws.clone();
    let state_for_turn = state.clone();
    let ledger_for_turn = ledger.clone();
    let contracts_for_turn = contracts.clone();
    let session_id = params.session_id.clone();
    let turn_id = params.turn_id.clone();
    let turn_state = Arc::new(TokioMutex::new(TurnState::Active));
    let (interrupt_tx, interrupt_rx) = mpsc::channel::<()>(1);
    let interrupt_tx = Arc::new(TokioMutex::new(Some(interrupt_tx)));
    let turn_state_for_task = turn_state.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        if let Some(fixture) = fixture {
            run_m9_fixture_turn(
                ws_for_turn,
                state_for_turn,
                ledger_for_turn,
                contracts_for_turn,
                params,
                fixture,
                turn_state_for_task,
                interrupt_rx,
            )
            .await;
        } else {
            run_standalone_turn(
                ws_for_turn,
                state_for_turn,
                ledger_for_turn,
                contracts_for_turn,
                features,
                params,
                prompt,
                turn_state_for_task,
                interrupt_rx,
            )
            .await;
        }
    });

    let inserted = {
        let mut active = active_turns.lock().await;
        // Allow replacing a `Terminal(_)` entry — the prior turn is finished;
        // we keep the entry only so a follow-up `turn/interrupt` can return
        // `terminal_state` instead of `unknown_turn`. Any non-terminal entry
        // means there is still a turn running for this session.
        let occupied = match active.get(&session_id) {
            Some(existing) => {
                let existing_state = existing.state.lock().await;
                !matches!(*existing_state, TurnState::Terminal(_))
            }
            None => false,
        };
        if occupied {
            false
        } else {
            active.insert(
                session_id.clone(),
                ActiveTurn {
                    turn_id: turn_id.clone(),
                    state: turn_state.clone(),
                    interrupt_tx,
                    abort: handle.abort_handle(),
                },
            );
            true
        }
    };
    if !inserted {
        handle.abort();
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_request("a turn is already running for this session"),
        );
        return;
    }

    connection_turns
        .lock()
        .await
        .insert(session_id, turn_id.clone());
    // Lifecycle reply: if the client cannot receive the accept, abort the
    // freshly-inserted turn — running an unaccepted turn would be a leak.
    if send_rpc_result(ws, id, json!({ "accepted": true })).is_err() {
        handle.abort();
        return;
    }
    let _ = start_tx.send(());
}

async fn handle_turn_interrupt(
    ws: &WsConnection,
    _ledger: &Arc<UiProtocolLedger>,
    active_turns: &SharedActiveTurns,
    // FIX-06 + FIX-08: kept on the signature so callers don't need to know
    // whether this handler currently evicts scopes / drains approvals itself.
    // The actual eviction + pending-approval cancel happens when
    // `run_standalone_turn` observes the interrupt: it calls
    // `cancel_pending_for_turn` (FIX-08) before `try_emit_terminal`
    // (FIX-03) and `evict_turn` (FIX-06) on exit. Centralising both there
    // guarantees a single happens-before edge: agent abort → cancel
    // notifications → terminal `turn/error code=interrupted`, all on the
    // same task that owned the turn.
    _contracts: &Arc<UiProtocolContractStores>,
    id: String,
    params: TurnInterruptParams,
) {
    let outcome = decide_interrupt(active_turns, &params).await;
    match outcome {
        InterruptOutcome::Unknown => {
            let _ = send_rpc_error(ws, Some(id), unknown_turn_error(&params.turn_id));
        }
        InterruptOutcome::Mismatch => {
            let _ = send_rpc_result(
                ws,
                id,
                json!({ "interrupted": false, "reason": "turn_id_mismatch" }),
            );
        }
        InterruptOutcome::AlreadyTerminal(reason) => {
            let interrupted = matches!(reason, TerminalReason::Interrupted);
            let _ = send_rpc_result(
                ws,
                id,
                json!({
                    "interrupted": interrupted,
                    "terminal_state": reason.as_str(),
                }),
            );
        }
        InterruptOutcome::AlreadyInterrupting => {
            // A prior caller transitioned the turn to `Interrupting` and is
            // awaiting ack. The terminal event is already guaranteed to be
            // emitted exactly once. Idempotent: report the same response shape
            // as the original caller will.
            let _ = send_rpc_result(ws, id, json!({ "interrupted": true }));
        }
        InterruptOutcome::Captured { ack_rx } => {
            // State is now `Interrupting { ack }`; the turn task is wired to
            // observe `interrupt_rx`, abort its agent, emit exactly one
            // `TurnError(interrupted)`, and signal `ack`. We do NOT abort the
            // outer turn future here — that would race with the terminal
            // emission and could lose the wire-side event.
            let result = tokio::time::timeout(INTERRUPT_ACK_TIMEOUT, ack_rx).await;
            let payload = match result {
                Ok(Ok(())) => json!({ "interrupted": true }),
                Ok(Err(_)) => {
                    // Sender dropped without ack — the task panicked or was
                    // cancelled before reaching the terminal arm. The state
                    // remains `Interrupting`; report timeout-style result so
                    // the caller knows the wire-side terminal is uncertain.
                    json!({ "interrupted": true, "ack_timeout": true })
                }
                Err(_) => json!({ "interrupted": true, "ack_timeout": true }),
            };
            let _ = send_rpc_result(ws, id, payload);
        }
    }
}

#[derive(Debug)]
enum InterruptOutcome {
    Unknown,
    Mismatch,
    AlreadyTerminal(TerminalReason),
    AlreadyInterrupting,
    Captured { ack_rx: oneshot::Receiver<()> },
}

async fn decide_interrupt(
    active_turns: &SharedActiveTurns,
    params: &TurnInterruptParams,
) -> InterruptOutcome {
    let registry = active_turns.lock().await;
    let Some(active) = registry.get(&params.session_id) else {
        return InterruptOutcome::Unknown;
    };
    if active.turn_id != params.turn_id {
        return InterruptOutcome::Mismatch;
    }

    // The lock boundary: hold the per-turn state mutex across the read and the
    // write. This is what closes the original TOCTOU window — natural
    // completion inside `run_standalone_turn` is gated on this same mutex via
    // `try_emit_terminal`, so the two paths can't both transition `Active` →
    // a terminal state.
    let state_arc = active.state.clone();
    let interrupt_tx_arc = active.interrupt_tx.clone();
    drop(registry);

    let mut state = state_arc.lock().await;
    match &*state {
        TurnState::Terminal(reason) => InterruptOutcome::AlreadyTerminal(*reason),
        TurnState::Interrupting { .. } => InterruptOutcome::AlreadyInterrupting,
        TurnState::Active => {
            let (ack_tx, ack_rx) = oneshot::channel();
            *state = TurnState::Interrupting { ack: ack_tx };
            drop(state);
            // Best-effort signal — capacity-1 channel; sending fails only if
            // the receiver has already been dropped (turn task is gone). Even
            // if the signal is lost, the state is already `Interrupting`, and
            // the next progress event in the task loop checks the state.
            let interrupt_tx = interrupt_tx_arc.lock().await.take();
            if let Some(tx) = interrupt_tx {
                let _ = tx.try_send(());
            }
            InterruptOutcome::Captured { ack_rx }
        }
    }
}

fn unknown_turn_error(turn_id: &TurnId) -> RpcError {
    let turn_id_str = turn_id.0.to_string();
    RpcError::new(UNKNOWN_TURN_CODE, format!("unknown turn: {turn_id_str}"))
        .with_data(json!({ "turn_id": turn_id_str, "kind": "unknown_turn" }))
}

async fn handle_approval_respond(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    connection_profile_id: Option<&str>,
    id: String,
    params: octos_core::ui_protocol::ApprovalRespondParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }

    let session_id = params.session_id.clone();
    let scope_string = params.approval_scope.clone();
    // FIX-01: `ApprovalDecision` is non-Copy because of the `Unknown(String)`
    // variant; clone to keep the value alive across `respond_with_context`
    // (consumes `params` via clone), the scope-recording call below, and the
    // FIX-07 audit/notification emission.
    let decision = params.decision.clone();

    let outcome = match contracts.approvals.respond_with_context(params.clone()) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };

    // FIX-06: if the user picked a recordable scope and we have the original
    // request context, register the policy entry. Open-registry rule:
    // unknown scope strings collapse to `approve_once` and are not recorded
    // — preserving backward compat with clients that send future scope
    // tokens we don't yet recognise.
    if let (Some(scope_string), Some(context)) = (scope_string.as_deref(), outcome.context.as_ref())
    {
        let scope_kind = ApprovalScopeKind::from_scope_str(scope_string);
        if scope_kind.is_recordable() {
            let match_key = match_key_for(scope_kind, &context.tool_name, &context.turn_id);
            contracts
                .scopes
                .record(&session_id, scope_kind, match_key, decision);
        }
    }

    let result = match serde_json::to_value(&outcome.result) {
        Ok(value) => value,
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!(
                    "failed to serialize approval/respond result: {error}"
                )),
            );
            return;
        }
    };
    let _ = send_rpc_result(ws, id, result);

    // FIX-07: audit + tracing + durable `approval/decided` ledger event.
    // `decided_by` carries the authenticated profile id when present;
    // empty means system-decided (matches the spec).
    //
    // For manual decisions (this path), `auto_resolved` stays `false`. The
    // auto-resolved emission lives in `UiProtocolApprovalRequester::request_approval`
    // for FIX-06's scope-policy short-circuit.
    let tool_name = outcome.context.as_ref().map(|ctx| ctx.tool_name.clone());
    let event = super::ui_protocol_approvals::build_decided_event(
        &params,
        &outcome,
        connection_profile_id.unwrap_or(""),
        Utc::now(),
    );
    log_decision_tracing(&event, tool_name.as_deref());

    if let Some(sessions) = state.sessions.as_ref() {
        let data_dir = sessions.lock().await.data_dir();
        let audit = contracts.audit_log(&data_dir);
        if let Err(error) = audit.record(&event, tool_name.as_deref()) {
            tracing::warn!(
                target: "octos.approvals.decision",
                approval_id = %event.approval_id.0,
                error = %error,
                "failed to append approval audit log entry"
            );
        }
    }

    let _ = send_notification_durable(ws, ledger, UiNotification::ApprovalDecided(event));
}

async fn handle_approval_scopes_list(
    ws: &WsConnection,
    scopes: &ScopePolicy,
    connection_profile_id: Option<&str>,
    id: String,
    params: octos_core::ui_protocol::ApprovalScopesListParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }

    let result = octos_core::ui_protocol::ApprovalScopesListResult {
        scopes: scopes.list_for_session(&params.session_id),
    };
    match serde_json::to_value(result) {
        Ok(result) => {
            let _ = send_rpc_result(ws, id, result);
        }
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!(
                    "failed to serialize approval/scopes/list result: {error}"
                )),
            );
        }
    }
}

async fn handle_diff_preview_get(
    ws: &WsConnection,
    diff_previews: &PendingDiffPreviewStore,
    connection_profile_id: Option<&str>,
    id: String,
    params: octos_core::ui_protocol::DiffPreviewGetParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }

    match diff_previews.get(params) {
        Ok(result) => match serde_json::to_value(result) {
            Ok(result) => {
                let _ = send_rpc_result(ws, id, result);
            }
            Err(error) => {
                let _ = send_rpc_error(
                    ws,
                    Some(id),
                    RpcError::internal_error(format!(
                        "failed to serialize diff/preview/get result: {error}"
                    )),
                );
            }
        },
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn handle_task_output_read(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    id: String,
    params: octos_core::ui_protocol::TaskOutputReadParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }

    match ui_protocol_task_output::read_task_output(state, params).await {
        Ok(result) => match serde_json::to_value(result) {
            Ok(result) => {
                let _ = send_rpc_result(ws, id, result);
            }
            Err(error) => {
                let _ = send_rpc_error(
                    ws,
                    Some(id),
                    RpcError::internal_error(format!(
                        "failed to serialize task/output/read result: {error}"
                    )),
                );
            }
        },
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn handle_task_list(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    id: String,
    params: TaskListParams,
) {
    let query_session_id =
        session_key_with_optional_topic(&params.session_id, params.topic.as_deref());
    if let Err(error) = validate_session_scope(&query_session_id, None, connection_profile_id) {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }

    match task_list_snapshot(state, &query_session_id) {
        Ok(tasks) => {
            let result = TaskListResult {
                session_id: params.session_id,
                topic: params.topic,
                tasks,
            };
            send_serialized_rpc_result(ws, id, octos_core::ui_protocol::methods::TASK_LIST, result);
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn handle_task_cancel(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    id: String,
    params: TaskCancelParams,
) {
    let Some(session_id) = params.session_id.as_ref() else {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params("task/cancel requires session_id for scoped cancellation"),
        );
        return;
    };
    if let Err(error) = validate_session_scope(
        session_id,
        params.profile_id.as_deref(),
        connection_profile_id,
    ) {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }

    let store = match task_query_store_or_error(state) {
        Ok(store) => store,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    let task_id = params.task_id.clone();
    match ensure_task_in_session(state, session_id, &task_id).and_then(|()| {
        store
            .cancel_task(&task_id.to_string())
            .map_err(|error| task_cancel_rpc_error(&task_id, error))
    }) {
        Ok(()) => {
            let result = TaskCancelResult {
                task_id,
                status: UiTaskRuntimeState::Cancelled,
            };
            send_serialized_rpc_result(
                ws,
                id,
                octos_core::ui_protocol::methods::TASK_CANCEL,
                result,
            );
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn handle_task_restart_from_node(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    id: String,
    params: TaskRestartFromNodeParams,
) {
    let Some(session_id) = params.session_id.as_ref() else {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(
                "task/restart_from_node requires session_id for scoped restart",
            ),
        );
        return;
    };
    if let Err(error) = validate_session_scope(
        session_id,
        params.profile_id.as_deref(),
        connection_profile_id,
    ) {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }

    let store = match task_query_store_or_error(state) {
        Ok(store) => store,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    let task_id = params.task_id.clone();
    let from_node = params.node_id.clone();
    let opts = octos_agent::RelaunchOpts {
        from_node: from_node.clone(),
    };
    match ensure_task_in_session(state, session_id, &task_id).and_then(|()| {
        store
            .relaunch_task(&task_id.to_string(), opts)
            .map_err(|error| task_relaunch_rpc_error(&task_id, error))
    }) {
        Ok(new_task_id) => {
            let new_task_id = match new_task_id.parse::<TaskId>() {
                Ok(task_id) => task_id,
                Err(error) => {
                    let _ = send_rpc_error(
                        ws,
                        Some(id),
                        RpcError::internal_error(format!(
                            "task supervisor returned an invalid relaunched task id: {error}"
                        )),
                    );
                    return;
                }
            };
            let result = TaskRestartFromNodeResult {
                original_task_id: task_id,
                new_task_id,
                from_node,
            };
            send_serialized_rpc_result(
                ws,
                id,
                octos_core::ui_protocol::methods::TASK_RESTART_FROM_NODE,
                result,
            );
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

fn send_serialized_rpc_result<T: Serialize>(
    ws: &WsConnection,
    id: String,
    method: &str,
    result: T,
) {
    match serde_json::to_value(result) {
        Ok(result) => {
            let _ = send_rpc_result(ws, id, result);
        }
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!("failed to serialize {method} result: {error}")),
            );
        }
    }
}

fn task_query_store_or_error(
    state: &Arc<AppState>,
) -> Result<&crate::session_actor::SessionTaskQueryStore, RpcError> {
    state.task_query_store.as_ref().ok_or_else(|| {
        RpcError::runtime_not_ready("task supervisor not wired for AppUI task commands")
            .with_data(json!({ "kind": "runtime_unavailable" }))
    })
}

fn task_list_snapshot(
    state: &Arc<AppState>,
    session_id: &SessionKey,
) -> Result<Vec<TaskListEntry>, RpcError> {
    let store = task_query_store_or_error(state)?;
    match store.query_json(&session_id.to_string()) {
        Value::Array(tasks) => tasks
            .into_iter()
            .map(task_list_entry_from_value)
            .collect::<Result<Vec<_>, _>>(),
        _ => Err(RpcError::internal_error(
            "task supervisor query returned a non-array task snapshot",
        )),
    }
}

fn session_key_with_optional_topic(session_id: &SessionKey, topic: Option<&str>) -> SessionKey {
    let Some(topic) = topic.map(str::trim).filter(|topic| !topic.is_empty()) else {
        return session_id.clone();
    };
    SessionKey(format!("{}#{topic}", session_id.base_key()))
}

#[derive(serde::Deserialize)]
struct TaskListProjection {
    id: TaskId,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_call_id: String,
    #[serde(default)]
    parent_session_key: Option<SessionKey>,
    #[serde(default)]
    child_session_key: Option<SessionKey>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    lifecycle_state: String,
    #[serde(default)]
    runtime_state: String,
    #[serde(default)]
    child_terminal_state: Option<String>,
    #[serde(default)]
    child_join_state: Option<String>,
    #[serde(default)]
    child_joined_at: Option<DateTime<Utc>>,
    #[serde(default)]
    child_failure_action: Option<String>,
    #[serde(default)]
    runtime_detail: Option<Value>,
    #[serde(default)]
    workflow_kind: Option<String>,
    #[serde(default)]
    current_phase: Option<String>,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    output_files: Vec<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    session_key: Option<SessionKey>,
}

fn task_list_entry_from_value(value: Value) -> Result<TaskListEntry, RpcError> {
    let projected: TaskListProjection = serde_json::from_value(value)
        .map_err(|error| RpcError::internal_error(format!("invalid task snapshot: {error}")))?;
    let state = ui_task_state_from_label(&projected.lifecycle_state)
        .or_else(|| ui_task_state_from_label(&projected.runtime_state))
        .or_else(|| ui_task_state_from_label(&projected.status))
        .unwrap_or(UiTaskRuntimeState::Running);

    Ok(TaskListEntry {
        id: projected.id,
        tool_name: projected.tool_name,
        tool_call_id: projected.tool_call_id,
        state,
        status: projected.status,
        lifecycle_state: projected.lifecycle_state,
        runtime_state: projected.runtime_state,
        parent_session_key: projected.parent_session_key,
        child_session_key: projected.child_session_key,
        child_terminal_state: projected.child_terminal_state,
        child_join_state: projected.child_join_state,
        child_joined_at: projected.child_joined_at,
        child_failure_action: projected.child_failure_action,
        runtime_detail: projected.runtime_detail,
        workflow_kind: projected.workflow_kind,
        current_phase: projected.current_phase,
        started_at: projected.started_at,
        updated_at: projected.updated_at,
        completed_at: projected.completed_at,
        output_files: projected.output_files,
        error: projected.error,
        session_key: projected.session_key,
    })
}

fn ui_task_state_from_label(label: &str) -> Option<UiTaskRuntimeState> {
    match label {
        "pending" | "queued" | "spawned" => Some(UiTaskRuntimeState::Pending),
        "running" | "executing_tool" | "resolving_outputs" | "verifying_outputs"
        | "delivering_outputs" | "cleaning_up" | "verifying" => Some(UiTaskRuntimeState::Running),
        "completed" | "ready" => Some(UiTaskRuntimeState::Completed),
        "failed" => Some(UiTaskRuntimeState::Failed),
        "cancelled" | "canceled" => Some(UiTaskRuntimeState::Cancelled),
        _ => None,
    }
}

fn ensure_task_in_session(
    state: &Arc<AppState>,
    session_id: &SessionKey,
    task_id: &TaskId,
) -> Result<(), RpcError> {
    if task_list_snapshot(state, session_id)?
        .iter()
        .any(|task| &task.id == task_id)
    {
        Ok(())
    } else {
        Err(RpcError::unknown_task_id(task_id))
    }
}

fn task_cancel_rpc_error(task_id: &TaskId, error: octos_agent::TaskCancelError) -> RpcError {
    match error {
        octos_agent::TaskCancelError::NotFound => RpcError::unknown_task_id(task_id),
        octos_agent::TaskCancelError::AlreadyTerminal => {
            RpcError::invalid_params("task is already terminal")
                .with_data(json!({ "kind": "task_already_terminal" }))
        }
    }
}

fn task_relaunch_rpc_error(task_id: &TaskId, error: octos_agent::TaskRelaunchError) -> RpcError {
    match error {
        octos_agent::TaskRelaunchError::NotFound => RpcError::unknown_task_id(task_id),
        octos_agent::TaskRelaunchError::StillActive => {
            RpcError::invalid_params("task is still active; cancel it before relaunching")
                .with_data(json!({ "kind": "task_still_active" }))
        }
    }
}

enum M9FixtureOutcome {
    Completed,
    Errored { code: &'static str, message: String },
    Interrupted,
}

async fn m9_fixture_delay_or_interrupt(
    interrupt_rx: &mut mpsc::Receiver<()>,
    duration: std::time::Duration,
) -> bool {
    tokio::select! {
        _ = interrupt_rx.recv() => true,
        _ = tokio::time::sleep(duration) => false,
    }
}

async fn run_m9_fixture_turn(
    ws: WsConnection,
    state: Arc<AppState>,
    ledger: Arc<UiProtocolLedger>,
    contracts: Arc<UiProtocolContractStores>,
    params: TurnStartParams,
    fixture: M9ProtocolFixture,
    turn_state: Arc<TokioMutex<TurnState>>,
    mut interrupt_rx: mpsc::Receiver<()>,
) {
    let session_id = params.session_id.clone();
    let turn_id = params.turn_id.clone();
    let started = UiNotification::TurnStarted(octos_core::ui_protocol::TurnStartedEvent {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        timestamp: Utc::now(),
    });
    if send_notification_lifecycle(&ws, &ledger, started).is_err() {
        let _ = transition_to_terminal(&turn_state, TerminalReason::Errored).await;
        contracts.scopes.evict_turn(&session_id, &turn_id);
        return;
    }
    let _ = send_notification_durable(
        &ws,
        &ledger,
        UiNotification::ProgressUpdated(UiProgressEvent::new(
            session_id.clone(),
            Some(turn_id.clone()),
            UiProgressMetadata::new(progress_kinds::STATUS).with_message("fixture turn running"),
        )),
    );

    let outcome = match fixture {
        M9ProtocolFixture::Basic => {
            let _ = send_notification_ephemeral(
                &ws,
                &ledger,
                UiNotification::MessageDelta(MessageDeltaEvent {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    text: "OK".to_owned(),
                }),
            );
            if m9_fixture_delay_or_interrupt(
                &mut interrupt_rx,
                std::time::Duration::from_millis(20),
            )
            .await
            {
                M9FixtureOutcome::Interrupted
            } else {
                M9FixtureOutcome::Completed
            }
        }
        M9ProtocolFixture::Slow => {
            let mut interrupted = false;
            for _ in 0..80 {
                let _ = send_notification_ephemeral(
                    &ws,
                    &ledger,
                    UiNotification::MessageDelta(MessageDeltaEvent {
                        session_id: session_id.clone(),
                        turn_id: turn_id.clone(),
                        text: "OK\n".to_owned(),
                    }),
                );
                if m9_fixture_delay_or_interrupt(
                    &mut interrupt_rx,
                    std::time::Duration::from_millis(25),
                )
                .await
                {
                    interrupted = true;
                    break;
                }
            }
            if interrupted {
                M9FixtureOutcome::Interrupted
            } else {
                M9FixtureOutcome::Completed
            }
        }
        M9ProtocolFixture::ToolEvents => {
            let tool_call_id = format!("m9-tool-{}", turn_id.0);
            let _ = send_notification_durable(
                &ws,
                &ledger,
                UiNotification::ToolStarted(ToolStartedEvent {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    tool_name: "list_dir".to_owned(),
                    arguments: Some(json!({ "path": "." })),
                }),
            );
            let _ = send_notification_durable(
                &ws,
                &ledger,
                UiNotification::ToolProgress(ToolProgressEvent {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    message: Some("listing workspace".to_owned()),
                    progress_pct: Some(50.0),
                }),
            );
            let _ = send_notification_durable(
                &ws,
                &ledger,
                UiNotification::ToolCompleted(ToolCompletedEvent {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    tool_call_id,
                    tool_name: "list_dir".to_owned(),
                    success: Some(true),
                    output_preview: Some("deterministic fixture listing".to_owned()),
                    duration_ms: Some(1),
                }),
            );
            if m9_fixture_delay_or_interrupt(
                &mut interrupt_rx,
                std::time::Duration::from_millis(20),
            )
            .await
            {
                M9FixtureOutcome::Interrupted
            } else {
                M9FixtureOutcome::Completed
            }
        }
        M9ProtocolFixture::Approval => {
            let approval_id = ApprovalId::new();
            let mut request = ApprovalRequestedEvent::generic(
                session_id.clone(),
                approval_id.clone(),
                turn_id.clone(),
                "shell",
                "M9 approval fixture",
                "printf m9-approval-e2e",
            );
            request.approval_kind = Some(approval_kinds::COMMAND.to_owned());
            request.risk = Some("low".to_owned());
            request.typed_details = Some(ApprovalTypedDetails::command(
                ApprovalCommandDetails {
                    argv: vec!["printf".to_owned(), "m9-approval-e2e".to_owned()],
                    command_line: Some("printf m9-approval-e2e".to_owned()),
                    cwd: None,
                    env_keys: Vec::new(),
                    tool_call_id: Some(format!("m9-approval-{}", turn_id.0)),
                },
                None,
            ));
            let response_rx = contracts.approvals.request_runtime(request.clone());
            if let Err(error) =
                send_notification_durable(&ws, &ledger, UiNotification::ApprovalRequested(request))
            {
                cancel_approval_after_request_send_failure(
                    contracts.as_ref(),
                    &ws,
                    &ledger,
                    &session_id,
                    &approval_id,
                    &turn_id,
                );
                M9FixtureOutcome::Errored {
                    code: "approval_send_failed",
                    message: format!("approval/requested notification not delivered: {error:?}"),
                }
            } else {
                tokio::select! {
                    _ = interrupt_rx.recv() => M9FixtureOutcome::Interrupted,
                    decision = response_rx => {
                        let text = match decision.unwrap_or(ApprovalDecision::Deny) {
                            ApprovalDecision::Approve => "approval approved",
                            ApprovalDecision::Deny | ApprovalDecision::Unknown(_) => "approval denied",
                        };
                        let _ = send_notification_ephemeral(
                            &ws,
                            &ledger,
                            UiNotification::MessageDelta(MessageDeltaEvent {
                                session_id: session_id.clone(),
                                turn_id: turn_id.clone(),
                                text: text.to_owned(),
                            }),
                        );
                        M9FixtureOutcome::Completed
                    }
                }
            }
        }
        M9ProtocolFixture::ReplayLossy => {
            ws.metrics.dropped_count.fetch_add(1, Ordering::Relaxed);
            emit_replay_lossy_opportunistic(&ws, &ledger, &session_id.0);
            if m9_fixture_delay_or_interrupt(
                &mut interrupt_rx,
                std::time::Duration::from_millis(20),
            )
            .await
            {
                M9FixtureOutcome::Interrupted
            } else {
                M9FixtureOutcome::Completed
            }
        }
        M9ProtocolFixture::TaskOutput => {
            match seed_m9_task_output_fixture(state.as_ref(), &session_id).await {
                Ok(task_id) => {
                    let _ = send_notification_durable(
                        &ws,
                        &ledger,
                        UiNotification::TaskUpdated(TaskUpdatedEvent {
                            session_id: session_id.clone(),
                            task_id: task_id.clone(),
                            title: "M9 task output fixture".to_owned(),
                            state: UiTaskRuntimeState::Running,
                            runtime_detail: Some(
                                "persisted deterministic task snapshot".to_owned(),
                            ),
                        }),
                    );
                    let _ = send_notification_durable(
                        &ws,
                        &ledger,
                        UiNotification::TaskOutputDelta(TaskOutputDeltaEvent {
                            session_id: session_id.clone(),
                            task_id: task_id.clone(),
                            cursor: OutputCursor { offset: 0 },
                            text: "fixture output line one\nfixture output line two\n".to_owned(),
                        }),
                    );
                    let _ = send_notification_durable(
                        &ws,
                        &ledger,
                        UiNotification::TaskUpdated(TaskUpdatedEvent {
                            session_id: session_id.clone(),
                            task_id,
                            title: "M9 task output fixture".to_owned(),
                            state: UiTaskRuntimeState::Completed,
                            runtime_detail: Some("fixture complete".to_owned()),
                        }),
                    );
                    if m9_fixture_delay_or_interrupt(
                        &mut interrupt_rx,
                        std::time::Duration::from_millis(20),
                    )
                    .await
                    {
                        M9FixtureOutcome::Interrupted
                    } else {
                        M9FixtureOutcome::Completed
                    }
                }
                Err(message) => M9FixtureOutcome::Errored {
                    code: "task_fixture_failed",
                    message,
                },
            }
        }
    };

    match outcome {
        M9FixtureOutcome::Completed => {
            try_emit_terminal(
                &turn_state,
                TerminalReason::Completed,
                &ws,
                &ledger,
                &session_id,
                &turn_id,
                None,
            )
            .await;
        }
        M9FixtureOutcome::Errored { code, message } => {
            try_emit_terminal(
                &turn_state,
                TerminalReason::Errored,
                &ws,
                &ledger,
                &session_id,
                &turn_id,
                Some((code, message.as_str())),
            )
            .await;
        }
        M9FixtureOutcome::Interrupted => {
            let cancelled = contracts.approvals.cancel_pending_for_turn(
                &session_id,
                &turn_id,
                approval_cancelled_reasons::TURN_INTERRUPTED,
            );
            for entry in cancelled {
                let _ = send_notification_durable(
                    &ws,
                    &ledger,
                    UiNotification::ApprovalCancelled(ApprovalCancelledEvent::turn_interrupted(
                        session_id.clone(),
                        entry.approval_id,
                        entry.turn_id,
                    )),
                );
            }
            try_emit_terminal(
                &turn_state,
                TerminalReason::Interrupted,
                &ws,
                &ledger,
                &session_id,
                &turn_id,
                Some(("interrupted", "turn interrupted by client")),
            )
            .await;
        }
    }

    contracts.scopes.evict_turn(&session_id, &turn_id);
}

async fn seed_m9_task_output_fixture(
    state: &AppState,
    session_id: &SessionKey,
) -> Result<TaskId, String> {
    let Some(sessions) = &state.sessions else {
        return Err("Sessions not available".to_owned());
    };
    let (data_dir, session_path) = {
        let mut sessions = sessions.lock().await;
        sessions.get_or_create(session_id).await;
        (sessions.data_dir(), sessions.session_path(session_id))
    };
    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create session dir: {error}"))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&session_path)
        .map_err(|error| format!("failed to materialize session file: {error}"))?;

    let supervisor = octos_agent::TaskSupervisor::new();
    supervisor
        .enable_persistence(ui_protocol_task_output::task_state_path(
            &data_dir, session_id,
        ))
        .map_err(|error| format!("failed to enable task persistence: {error}"))?;
    let task_id = supervisor.register("shell", "m9-task-output-fixture", Some(&session_id.0));
    supervisor.mark_running(&task_id);
    supervisor.mark_runtime_state(
        &task_id,
        octos_agent::TaskRuntimeState::DeliveringOutputs,
        Some(
            json!({
                "workflow_kind": "m9_fixture",
                "current_phase": "collecting_output",
                "progress_message": "Collecting deterministic fixture output"
            })
            .to_string(),
        ),
    );
    supervisor.mark_failed(
        &task_id,
        "fixture output line one\nfixture output line two\nfixture output line three\n".to_owned(),
    );
    task_id
        .parse::<TaskId>()
        .map_err(|error| format!("failed to parse fixture task id: {error}"))
}

async fn run_standalone_turn(
    ws: WsConnection,
    state: Arc<AppState>,
    ledger: Arc<UiProtocolLedger>,
    contracts: Arc<UiProtocolContractStores>,
    features: ConnectionUiFeatures,
    params: TurnStartParams,
    prompt: String,
    turn_state: Arc<TokioMutex<TurnState>>,
    mut interrupt_rx: mpsc::Receiver<()>,
) {
    let session_id = params.session_id.clone();
    let turn_id = params.turn_id.clone();
    let started = UiNotification::TurnStarted(octos_core::ui_protocol::TurnStartedEvent {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        timestamp: Utc::now(),
    });
    // turn/started is lifecycle. If the client cannot receive it we may as
    // well stop now — the rest of the turn is wasted work. Per FIX-03,
    // transition the turn to a terminal state so the registry doesn't keep
    // an orphaned `Active` entry.
    if send_notification_lifecycle(&ws, &ledger, started).is_err() {
        let _ = transition_to_terminal(&turn_state, TerminalReason::Errored).await;
        contracts.scopes.evict_turn(&session_id, &turn_id);
        return;
    }

    let (base_agent, sessions) = match validate_runtime(&state) {
        Ok(runtime) => runtime,
        Err(error) => {
            try_emit_terminal(
                &turn_state,
                TerminalReason::Errored,
                &ws,
                &ledger,
                &session_id,
                &turn_id,
                Some(("runtime_unavailable", error.as_str())),
            )
            .await;
            // FIX-06: a turn that ends — for any reason — must drop its
            // `approve_for_turn` policy entries so a subsequent turn can't
            // reuse them.
            contracts.scopes.evict_turn(&session_id, &turn_id);
            return;
        }
    };

    let history: Vec<Message> = {
        let mut sessions = sessions.lock().await;
        let session = sessions.get_or_create(&session_id).await;
        session.get_history(50).to_vec()
    };

    let (tool_registry, workspace_root) =
        match session_tool_registry(base_agent.as_ref(), &session_id) {
            Ok(registry) => registry,
            Err(error) => {
                // FIX-03 pattern: terminal emission + state transition is atomic.
                try_emit_terminal(
                    &turn_state,
                    TerminalReason::Errored,
                    &ws,
                    &ledger,
                    &session_id,
                    &turn_id,
                    Some(("cwd_binding_failed", error.as_str())),
                )
                .await;
                contracts.scopes.evict_turn(&session_id, &turn_id);
                return;
            }
        };
    let progress_workspace_root = workspace_root
        .clone()
        .or_else(|| tool_registry.workspace_root().map(Path::to_path_buf));

    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::channel::<String>(PROGRESS_CHANNEL_CAPACITY);
    let progress_dropped = Arc::new(AtomicU64::new(0));
    let reporter: Arc<dyn octos_agent::ProgressReporter> =
        Arc::new(MetricsReporter::new(Arc::new(BoundedChannelReporter::new(
            progress_tx.clone(),
            progress_dropped.clone(),
        ))));
    let progress_tx_for_result = progress_tx.clone();
    let progress_tx_for_tasks = progress_tx.clone();
    let task_progress_dropped = progress_dropped.clone();
    tool_registry.supervisor().set_on_change(move |task| {
        // M9-06: terminal updates (completed/failed/cancelled) must not be
        // dropped under WebSocket backpressure — dropping one would leave the
        // UI stuck on `running` indefinitely. See
        // `forward_task_progress_to_channel`.
        forward_task_progress_to_channel(&progress_tx_for_tasks, &task_progress_dropped, task);
    });
    drop(progress_tx);
    let request_agent = Agent::new_shared(
        AgentId::new(format!("ui-protocol-{}", uuid::Uuid::now_v7())),
        base_agent.llm_provider(),
        tool_registry,
        base_agent.memory_store(),
    )
    .with_config(base_agent.agent_config())
    .with_system_prompt(session_system_prompt(
        base_agent.as_ref(),
        workspace_root.as_deref(),
    ))
    .with_reporter(reporter);

    let agent_session_id = session_id.clone();
    let approval_requester: Arc<dyn octos_agent::ToolApprovalRequester> =
        Arc::new(UiProtocolApprovalRequester {
            ws: ws.clone(),
            ledger: ledger.clone(),
            contracts: contracts.clone(),
            state: state.clone(),
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            features,
        });
    let agent_task = tokio::spawn(async move {
        let result = octos_agent::tools::TOOL_APPROVAL_CTX
            .scope(
                approval_requester,
                request_agent.process_message(&prompt, &history, Vec::new()),
            )
            .await;

        match result {
            Ok(response) => {
                let mut cursor = None;
                {
                    let mut sessions = sessions.lock().await;
                    let final_assistant = final_assistant_message(
                        &response.messages,
                        &response.content,
                        response.reasoning_content.clone(),
                    );
                    for message in response.messages.iter().cloned().chain(final_assistant) {
                        if let Ok(seq) = sessions
                            .add_message_with_seq(&agent_session_id, message)
                            .await
                        {
                            cursor = Some(UiCursor {
                                stream: agent_session_id.0.clone(),
                                seq: seq as u64,
                            });
                        }
                    }
                }
                let done = json!({
                    "type": "done",
                    "content": response.content,
                    "tokens_in": response.token_usage.input_tokens,
                    "tokens_out": response.token_usage.output_tokens,
                    "cursor": cursor,
                });
                let _ = progress_tx_for_result.send(done.to_string()).await;
            }
            Err(error) => {
                let error = json!({
                    "type": "error",
                    "message": error.to_string(),
                });
                let _ = progress_tx_for_result.send(error.to_string()).await;
            }
        }
    });
    let _abort_guard = AbortOnDrop {
        abort: agent_task.abort_handle(),
    };

    let mut saw_delta = false;
    let mut task_output_delta_tracker = TaskOutputDeltaTracker::default();
    let progress_context = ProgressMappingContext::new(session_id.clone(), turn_id.clone());
    let mut interrupt_observed = false;
    loop {
        // Race progress events against the interrupt signal so an interrupt
        // can wake us out of `progress_rx.recv()` even if the agent task is
        // mid-await. The state mutex is the actual race winner; this select
        // is a notification, not a guard.
        let event = tokio::select! {
            biased;
            _ = interrupt_rx.recv(), if !interrupt_observed => {
                interrupt_observed = true;
                continue;
            }
            recv = progress_rx.recv() => match recv {
                Some(data) => match serde_json::from_str::<Value>(&data) {
                    Ok(event) => event,
                    Err(_) => continue,
                },
                None => break,
            }
        };
        if interrupt_observed {
            // The handler transitioned state to `Interrupting`. Drop any
            // remaining progress events on the floor; they are no longer
            // observable to the client.
            break;
        }
        match event.get("type").and_then(Value::as_str) {
            Some("done") => {
                if !saw_delta {
                    if let Some(content) = event.get("content").and_then(Value::as_str) {
                        if !content.is_empty() {
                            // message/delta is ephemeral per spec § 9 — drops
                            // are silent at DEBUG.
                            let _ = send_notification_ephemeral(
                                &ws,
                                &ledger,
                                UiNotification::MessageDelta(MessageDeltaEvent {
                                    session_id: session_id.clone(),
                                    turn_id: turn_id.clone(),
                                    text: content.to_string(),
                                }),
                            );
                        }
                    }
                }
                // FIX-04: flush any accumulated drops before the lifecycle
                // terminal so the client knows the cursor is incomplete.
                flush_replay_lossy(&ws, &ledger, &session_id, &progress_dropped);
                try_emit_terminal(
                    &turn_state,
                    TerminalReason::Completed,
                    &ws,
                    &ledger,
                    &session_id,
                    &turn_id,
                    None,
                )
                .await;
                break;
            }
            Some("error") => {
                let message = event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("turn failed")
                    .to_string();
                flush_replay_lossy(&ws, &ledger, &session_id, &progress_dropped);
                try_emit_terminal(
                    &turn_state,
                    TerminalReason::Errored,
                    &ws,
                    &ledger,
                    &session_id,
                    &turn_id,
                    Some(("runtime_error", message.as_str())),
                )
                .await;
                break;
            }
            _ => {
                if let Some(delta) =
                    task_output_delta_tracker.observe_progress_event(&session_id, &event)
                {
                    // task/output/delta is durable: drops surface as
                    // protocol/replay_lossy so the client can resync.
                    let _ = send_notification_durable(
                        &ws,
                        &ledger,
                        UiNotification::TaskOutputDelta(delta),
                    );
                }
                let mut mapping = map_progress_json(&progress_context, &event);
                apply_progress_contract_side_effects(
                    &contracts,
                    &progress_context,
                    progress_workspace_root.as_deref(),
                    &event,
                    &mut mapping,
                );
                for notification in mapping.notifications {
                    match notification {
                        UiNotification::MessageDelta(_) => {
                            saw_delta = true;
                            let _ = send_notification_ephemeral(&ws, &ledger, notification);
                        }
                        UiNotification::ApprovalRequested(request) => {
                            if send_notification_durable(
                                &ws,
                                &ledger,
                                UiNotification::ApprovalRequested(request.clone()),
                            )
                            .is_err()
                            {
                                cancel_approval_after_request_send_failure(
                                    contracts.as_ref(),
                                    &ws,
                                    &ledger,
                                    &request.session_id,
                                    &request.approval_id,
                                    &request.turn_id,
                                );
                            }
                        }
                        notification => {
                            let _ = send_notification_durable(&ws, &ledger, notification);
                        }
                    }
                }
                if let Some(warning) = mapping.warning {
                    let _ =
                        send_notification_durable(&ws, &ledger, UiNotification::Warning(warning));
                }
                if let Some(status) = mapping.status {
                    let event = ledger.append_progress(status.event);
                    let _ = send_ledger_event_durable(&ws, &ledger, event.event);
                }
            }
        }
    }

    if interrupt_observed {
        // Stop the agent so any in-flight LLM/tool await unblocks promptly.
        agent_task.abort();
        // FIX-04: also flush any accumulated drops before the lifecycle
        // terminal so the client knows the cursor is incomplete.
        flush_replay_lossy(&ws, &ledger, &session_id, &progress_dropped);
        // FIX-08: drain pending approvals tied to the interrupted turn before
        // emitting the terminal `turn/error code=interrupted`. Ordering on the
        // wire/ledger:
        //   1. agent aborted (above) — no new requests will ever arrive.
        //   2. one `approval/cancelled` per still-pending approval (durable).
        //   3. exactly one `turn/error code=interrupted` (via try_emit_terminal).
        // This matches the FIX-08 spec: cancel events appear in the ledger
        // before the terminal, so reconnect-replay clients see "moot" before
        // they see "turn gone". `cancel_pending_for_turn` is atomic
        // (single write-lock over the per-call store) and idempotent (a
        // replayed interrupt finds nothing pending and returns []).
        //
        // FIX-06 interaction: this only touches per-call pending entries.
        // `approve_for_session` scopes are turn-independent and survive;
        // `approve_for_turn` scopes are evicted by `evict_turn` below.
        //
        // TODO(M9-FIX-07-followup): mirror each cancellation into the audit
        // log (`decision: "cancelled"`, `reason: "turn_interrupted"`). FIX-08
        // intentionally limits scope to the durable ledger path; the audit
        // tap can be added without re-reading the spec.
        let cancelled = contracts.approvals.cancel_pending_for_turn(
            &session_id,
            &turn_id,
            approval_cancelled_reasons::TURN_INTERRUPTED,
        );
        for entry in cancelled {
            let _ = send_notification_durable(
                &ws,
                &ledger,
                UiNotification::ApprovalCancelled(ApprovalCancelledEvent::turn_interrupted(
                    session_id.clone(),
                    entry.approval_id,
                    entry.turn_id,
                )),
            );
        }
        // Handler is awaiting our terminal emission + ack. Emit exactly once.
        try_emit_terminal(
            &turn_state,
            TerminalReason::Interrupted,
            &ws,
            &ledger,
            &session_id,
            &turn_id,
            Some(("interrupted", "turn interrupted by client")),
        )
        .await;
    }

    let _ = agent_task.await;
    // FIX-06: a turn that ends — for any reason — must drop its
    // `approve_for_turn` policy entries so a subsequent turn can't reuse
    // them. The state-machine entry itself is intentionally retained here
    // so a follow-up `turn/interrupt` for this `turn_id` can return
    // `{interrupted: false, terminal_state: "completed"}` instead of
    // `unknown_turn`. The entry is reaped on connection close
    // (`abort_connection_turns`) or when a new `turn/start` replaces it.
    contracts.scopes.evict_turn(&session_id, &turn_id);
}

/// Outcome of transitioning into a terminal state. `None` means we lost the
/// race — state was already terminal — and the caller must NOT emit anything.
struct TerminalTransition {
    /// The final terminal reason reflected on the wire. May differ from the
    /// caller's `expected` if state was `Interrupting`.
    reason: TerminalReason,
    /// Pending ack channel from a concurrent interrupt handler; signal after
    /// the wire-side emission completes.
    ack: Option<oneshot::Sender<()>>,
}

/// Atomically transition the turn state to `Terminal(_)` exactly once.
/// `Active` → `Terminal(expected)`. `Interrupting { ack }` →
/// `Terminal(Interrupted)` with `ack` for the caller to signal. `Terminal(_)`
/// is left intact — caller is the loser of a race and must not emit.
async fn transition_to_terminal(
    turn_state: &TokioMutex<TurnState>,
    expected: TerminalReason,
) -> Option<TerminalTransition> {
    let mut state = turn_state.lock().await;
    let (reason, ack) = match std::mem::replace(&mut *state, TurnState::Active) {
        TurnState::Active => (expected, None),
        TurnState::Interrupting { ack } => (TerminalReason::Interrupted, Some(ack)),
        TurnState::Terminal(prior) => {
            *state = TurnState::Terminal(prior);
            return None;
        }
    };
    *state = TurnState::Terminal(reason);
    Some(TerminalTransition { reason, ack })
}

/// Atomically transition state and emit exactly one terminal event. No-op if
/// the state is already `Terminal(_)`. See `transition_to_terminal` for the
/// state-machine details.
async fn try_emit_terminal(
    turn_state: &TokioMutex<TurnState>,
    expected_reason: TerminalReason,
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    turn_id: &TurnId,
    error_payload: Option<(&str, &str)>,
) {
    let Some(TerminalTransition { reason, ack }) =
        transition_to_terminal(turn_state, expected_reason).await
    else {
        return;
    };

    // Terminal events are lifecycle: failure to deliver does not change the
    // state-machine outcome (the entry stays terminal for replay/idempotency)
    // but the ledger is still appended so reconnect-replay can catch up.
    match reason {
        TerminalReason::Completed => {
            let _ = send_notification_lifecycle(
                ws,
                ledger,
                UiNotification::TurnCompleted(TurnCompletedEvent {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    cursor: None,
                }),
            );
        }
        TerminalReason::Errored => {
            let (code, message) = error_payload.unwrap_or(("runtime_error", "turn failed"));
            let _ = send_turn_error(ws, ledger, session_id, turn_id, code, message);
        }
        TerminalReason::Interrupted => {
            let (code, message) = error_payload.unwrap_or(("interrupted", "turn interrupted"));
            let _ = send_turn_error(ws, ledger, session_id, turn_id, code, message);
        }
    }

    if let Some(ack) = ack {
        let _ = ack.send(());
    }
}

fn apply_progress_contract_side_effects(
    contracts: &UiProtocolContractStores,
    context: &ProgressMappingContext,
    workspace_root: Option<&Path>,
    event: &Value,
    mapping: &mut UiProgressMapping,
) {
    for notification in mapping.notifications.iter_mut() {
        if let UiNotification::ApprovalRequested(request) = notification {
            harden_progress_emitted_approval(request);
            contracts.approvals.request(request.clone());
        }
    }

    let Some(status) = mapping.status.as_mut() else {
        return;
    };
    let Some(notice) = status.event.metadata.file_mutation.as_mut() else {
        return;
    };
    let explicit_diff = event.get("diff").and_then(Value::as_str);
    let materialized_diff = if explicit_diff.is_none() {
        materialize_file_mutation_diff(notice, workspace_root)
    } else {
        None
    };
    let diff = explicit_diff.or(materialized_diff.as_deref());
    // `diff_previews(None)` returns the singleton installed during
    // connection-open (durable when `state.sessions` is wired,
    // ephemeral otherwise). The store does its own write-ahead before
    // the in-memory map update.
    contracts.diff_previews(None).upsert_file_mutation(
        context.session_id.clone(),
        &context.turn_id,
        notice,
        diff,
    );
}

/// Harden an `ApprovalRequestedEvent` produced from a tool/progress payload.
///
/// Tools can emit their own `approval_requested` progress event, which
/// `map_approval_requested` lifts straight into a notification. Two
/// invariants must be enforced before the event lands in the pending
/// approval store or on the wire:
///
/// 1. Risk is always sourced from the manifest. A tool-claimed risk on the
///    upstream payload is logged at WARN and dropped — it would otherwise
///    let `rm_rf` self-attest as `low`.
/// 2. Path-shaped strings inside the typed details (`cwd`,
///    `filesystem.paths`, `filesystem.writable_roots`,
///    `sandbox.writable_roots`) are passed through `sanitize_display_path`
///    so RTL overrides, zero-width characters, and traversal sequences
///    cannot spoof the rendered path.
fn harden_progress_emitted_approval(event: &mut ApprovalRequestedEvent) {
    if let Some(claimed) = event.risk.as_deref() {
        tracing::warn!(
            tool = %event.tool_name,
            claimed_risk = %claimed,
            "tool-emitted approval risk is ignored; using manifest-declared risk"
        );
    }
    event.risk = Some(server_risk_for(&event.tool_name));

    let Some(typed) = event.typed_details.as_mut() else {
        return;
    };
    if let Some(command) = typed.command.as_mut() {
        if let Some(cwd) = command.cwd.as_deref() {
            command.cwd = Some(sanitize_display_path(cwd));
        }
    }
    if let Some(filesystem) = typed.filesystem.as_mut() {
        for path in filesystem.paths.iter_mut() {
            *path = sanitize_display_path(path);
        }
        for root in filesystem.writable_roots.iter_mut() {
            *root = sanitize_display_path(root);
        }
    }
    if let Some(sandbox) = typed.sandbox.as_mut() {
        for root in sandbox.writable_roots.iter_mut() {
            *root = sanitize_display_path(root);
        }
    }
}

fn materialize_file_mutation_diff(
    notice: &UiFileMutationNotice,
    workspace_root: Option<&Path>,
) -> Option<String> {
    let path = PathBuf::from(&notice.path);
    let absolute_path = if path.is_absolute() {
        path
    } else if let Some(workspace_root) = workspace_root {
        workspace_root.join(path)
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let git_root = find_git_root_for_path(&absolute_path)?;
    let relative_path = absolute_path.strip_prefix(&git_root).ok()?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&git_root)
        .arg("diff")
        .arg("--")
        .arg(relative_path)
        .output()
        .ok()?;

    if !output.status.success() && output.stdout.is_empty() {
        return None;
    }

    let diff = String::from_utf8(output.stdout).ok()?;
    let diff = truncate_utf8(diff.trim_end().to_owned(), MAX_DIFF_PREVIEW_BYTES);
    (!diff.is_empty()).then_some(diff)
}

fn find_git_root_for_path(path: &Path) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    start
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    value.truncate(boundary);
    value
}

fn prompt_text(input: &[InputItem]) -> Option<String> {
    let parts = input
        .iter()
        .filter_map(|item| match item {
            InputItem::Text { text } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
        .collect::<Vec<_>>();

    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn task_id_field(event: &Value) -> Option<TaskId> {
    event.get("task_id").and_then(Value::as_str)?.parse().ok()
}

fn task_output_delta_text(event: &Value) -> Option<String> {
    match event.get("type").and_then(Value::as_str)? {
        "tool_progress" | "task_progress" | "task_output" => string_field(
            event,
            &["text", "output", "progress_message", "message", "status"],
        ),
        "tool_end" => string_field(event, &["output_preview"]),
        _ => None,
    }
    .filter(|text| !text.is_empty())
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn validate_runtime(
    state: &AppState,
) -> Result<
    (
        Arc<Agent>,
        Arc<tokio::sync::Mutex<octos_bus::SessionManager>>,
    ),
    String,
> {
    let agent = state.agent.as_ref().ok_or_else(|| {
        "No LLM provider configured. Set up a profile with an API key first.".to_string()
    })?;
    let sessions = state
        .sessions
        .as_ref()
        .ok_or_else(|| "Sessions not available".to_string())?;
    Ok((agent.clone(), sessions.clone()))
}

fn runtime_unavailable_error(message: impl Into<String>) -> RpcError {
    RpcError::internal_error(message).with_data(json!({
        "kind": "runtime_unavailable",
    }))
}

fn final_assistant_message(
    messages: &[Message],
    content: &str,
    reasoning_content: Option<String>,
) -> Option<Message> {
    if content.is_empty()
        || messages
            .iter()
            .any(|message| message.role == MessageRole::Assistant && message.content == content)
    {
        return None;
    }

    let mut message = Message::assistant(content.to_owned());
    message.reasoning_content = reasoning_content;
    Some(message)
}

async fn abort_connection_turns(
    active_turns: &SharedActiveTurns,
    connection_turns: &SharedConnectionTurns,
    scopes: &ScopePolicy,
) {
    let turns = std::mem::take(&mut *connection_turns.lock().await);
    if turns.is_empty() {
        return;
    }

    let mut active = active_turns.lock().await;
    for (session_id, turn_id) in turns {
        let should_abort = active
            .get(&session_id)
            .is_some_and(|active| active.turn_id == turn_id);
        if should_abort {
            if let Some(active) = active.remove(&session_id) {
                active.abort.abort();
            }
        }
        // FIX-06: connection close is the de-facto "session close" hook in
        // v1alpha1 — drop every recorded scope for this session so it cannot
        // outlive the WebSocket. Per M9-FIX-06 § "Out of scope", an explicit
        // `session/close` wire event would be a cleaner trigger; until then
        // this best-effort hook is the canonical place.
        scopes.evict_session(&session_id);
    }
}

/// Build the wire frame for a JSON value, returning `None` and incrementing
/// the lifecycle-error counter on serialization failure (which only happens
/// when a payload contains non-serializable data; treat as lifecycle).
fn frame_for<T: serde::Serialize>(value: &T) -> Option<WsMessage> {
    match serde_json::to_string(value) {
        Ok(text) => Some(WsMessage::text(text)),
        Err(error) => {
            metrics::counter!("ws.send.error.lifecycle").increment(1);
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                error = %error,
                "failed to serialize ws frame"
            );
            None
        }
    }
}

fn send_turn_error(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    turn_id: &TurnId,
    code: impl Into<String>,
    message: impl Into<String>,
) -> Result<(), SendError> {
    send_notification_lifecycle(
        ws,
        ledger,
        UiNotification::TurnError(TurnErrorEvent {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            code: code.into(),
            message: message.into(),
        }),
    )
}

fn send_rpc_result(ws: &WsConnection, id: String, result: Value) -> Result<(), SendError> {
    let frame = frame_for(&RpcResponse::success(id, result))
        .ok_or_else(|| SendError::LifecycleFailure("rpc result serialization".into()))?;
    ws.send_lifecycle(frame)
}

fn send_rpc_error(ws: &WsConnection, id: Option<String>, error: RpcError) -> Result<(), SendError> {
    let frame = frame_for(&RpcErrorResponse::new(id, error))
        .ok_or_else(|| SendError::LifecycleFailure("rpc error serialization".into()))?;
    ws.send_lifecycle(frame)
}

fn send_notification_lifecycle(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    notification: UiNotification,
) -> Result<(), SendError> {
    let event = ledger.append_notification(notification);
    let cursor = event.cursor.clone();
    let method = ledger_event_method(&event.event).to_string();
    let frame = frame_from_ledger(event.event)
        .ok_or_else(|| SendError::LifecycleFailure(format!("serialize {method}")))?;
    match ws.send_lifecycle(frame) {
        Ok(()) => {
            ws.metrics.record_durable_cursor(&cursor);
            Ok(())
        }
        Err(SendError::LifecycleFailure(reason)) => {
            // The ledger entry stays — the spec calls this `delivery_failed`
            // from the caller's perspective (turn aborts cleanly).
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                method = %method,
                reason = %reason,
                "lifecycle notification not delivered; entry remains in ledger as delivery_failed"
            );
            Err(SendError::LifecycleFailure(reason))
        }
        Err(other) => Err(other),
    }
}

fn send_notification_durable(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    notification: UiNotification,
) -> Result<(), SendError> {
    let event = ledger.append_notification(notification);
    let cursor = event.cursor.clone();
    let method = ledger_event_method(&event.event).to_string();
    let frame = match frame_from_ledger(event.event) {
        Some(frame) => frame,
        None => {
            return Err(SendError::BackpressureDrop);
        }
    };
    match ws.send_durable(frame, &method) {
        Ok(()) => {
            ws.metrics.record_durable_cursor(&cursor);
            Ok(())
        }
        Err(SendError::BackpressureDrop) => {
            // Best-effort: try to tell the client right away. If even the
            // lossy frame cannot enqueue, accumulate and flush later.
            emit_replay_lossy_opportunistic(ws, ledger, &cursor.stream);
            Err(SendError::BackpressureDrop)
        }
        Err(other) => Err(other),
    }
}

fn send_notification_ephemeral(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    notification: UiNotification,
) -> Result<(), SendError> {
    // Ephemeral frames are NOT appended to the ledger — they are explicitly
    // non-durable per spec § 9. Drops never need a `replay_lossy` summary.
    let method = notification.method().to_string();
    let rpc = match notification.into_rpc_notification() {
        Ok(rpc) => rpc,
        Err(error) => {
            tracing::debug!(
                target: "octos::ui_protocol::ws",
                method = %method,
                error = %error,
                "failed to serialize ephemeral notification"
            );
            return Err(SendError::BackpressureDrop);
        }
    };
    let frame = frame_for(&rpc).ok_or(SendError::BackpressureDrop)?;
    let _ = ledger; // unused for ephemeral, kept for symmetry with durable
    ws.send_ephemeral(frame, &method)
}

fn send_ledger_event_durable(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    event: UiProtocolLedgerEvent,
) -> Result<(), SendError> {
    let method = ledger_event_method(&event).to_string();
    // `event` already carries its cursor (set by the ledger before storage)
    // — pull a copy out before consuming the event into a frame.
    let cursor = ledger_event_cursor(&event);
    let frame = match frame_from_ledger(event) {
        Some(frame) => frame,
        None => return Err(SendError::BackpressureDrop),
    };
    match ws.send_durable(frame, &method) {
        Ok(()) => {
            if let Some(cursor) = cursor {
                ws.metrics.record_durable_cursor(&cursor);
            }
            Ok(())
        }
        Err(SendError::BackpressureDrop) => {
            if let Some(cursor) = cursor.as_ref() {
                emit_replay_lossy_opportunistic(ws, ledger, &cursor.stream);
            }
            Err(SendError::BackpressureDrop)
        }
        Err(other) => Err(other),
    }
}

fn frame_from_ledger(event: UiProtocolLedgerEvent) -> Option<WsMessage> {
    let notification = match event.into_rpc_notification() {
        Ok(rpc) => rpc,
        Err(error) => {
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                error = %error,
                "ledger event failed to serialize"
            );
            return None;
        }
    };
    frame_for(&notification)
}

fn ledger_event_method(event: &UiProtocolLedgerEvent) -> &'static str {
    match event {
        UiProtocolLedgerEvent::Notification(n) => n.method(),
        UiProtocolLedgerEvent::Progress(_) => octos_core::ui_protocol::methods::PROGRESS_UPDATED,
    }
}

fn ledger_event_cursor(event: &UiProtocolLedgerEvent) -> Option<UiCursor> {
    match event {
        UiProtocolLedgerEvent::Notification(UiNotification::SessionOpened(SessionOpened {
            cursor: Some(cursor),
            ..
        })) => Some(cursor.clone()),
        UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(
            TurnCompletedEvent {
                cursor: Some(cursor),
                ..
            },
        )) => Some(cursor.clone()),
        _ => None,
    }
}

/// Best-effort: append a `protocol/replay_lossy` summary to the ledger and
/// try to enqueue it. Failures here are logged and discarded — the next
/// successful send will retry via `flush_replay_lossy`.
fn emit_replay_lossy_opportunistic(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_stream: &str,
) {
    let session_id = SessionKey(session_stream.to_string());
    let dropped = ws.metrics.dropped_count.swap(0, Ordering::Relaxed);
    if dropped == 0 {
        return;
    }
    let last_cursor = ws.metrics.snapshot_last_cursor();
    let lossy = UiNotification::ReplayLossy(ReplayLossyEvent {
        session_id,
        dropped_count: dropped,
        last_durable_cursor: last_cursor,
    });
    let event = ledger.append_notification(lossy);
    let method = octos_core::ui_protocol::methods::REPLAY_LOSSY.to_string();
    let frame = match frame_from_ledger(event.event) {
        Some(frame) => frame,
        None => return,
    };
    if ws.try_enqueue(frame).is_err() {
        // Channel is still full or closed. Push the count back and let the
        // next successful send opportunity flush it.
        ws.metrics
            .dropped_count
            .fetch_add(dropped, Ordering::Relaxed);
        tracing::warn!(
            target: "octos::ui_protocol::ws",
            method = %method,
            "replay_lossy could not be queued; will retry on next send"
        );
    }
}

/// Drain any accumulated drops as a final `protocol/replay_lossy` before a
/// turn boundary. Intended to be called just before `turn/completed` or
/// `turn/error` so the client knows the cursor is incomplete.
fn flush_replay_lossy(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    progress_dropped: &Arc<AtomicU64>,
) {
    let progress_drops = progress_dropped.swap(0, Ordering::Relaxed);
    if progress_drops > 0 {
        ws.metrics
            .dropped_count
            .fetch_add(progress_drops, Ordering::Relaxed);
    }
    if ws.metrics.dropped_count.load(Ordering::Relaxed) == 0 {
        return;
    }
    emit_replay_lossy_opportunistic(ws, ledger, &session_id.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_store::UserRole;
    use octos_core::ui_protocol::{
        ApprovalDecision, ApprovalId, ApprovalRespondParams, ApprovalRespondStatus, DiffPreview,
        DiffPreviewFile, DiffPreviewFileStatus, DiffPreviewGetParams, DiffPreviewGetStatus,
        DiffPreviewHunk, DiffPreviewLine, DiffPreviewLineKind, DiffPreviewSource, PreviewId,
        approval_scopes, methods, rpc_error_codes,
    };

    #[test]
    fn parses_turn_start_rpc_request() {
        let request = UiCommand::TurnStart(TurnStartParams {
            session_id: SessionKey("local:test".into()),
            turn_id: TurnId::new(),
            input: vec![InputItem::Text {
                text: "hello".into(),
            }],
        })
        .into_rpc_request("1")
        .expect("request");
        let text = serde_json::to_string(&request).expect("json");

        let decoded = parse_rpc_request(&text).expect("parse");

        assert_eq!(decoded.method, methods::TURN_START);
        assert_eq!(decoded.id, "1");
        assert!(matches!(
            route_rpc_command(decoded).expect("route"),
            UiCommand::TurnStart(_)
        ));
    }

    #[test]
    fn task_output_read_decodes_protocol_params() {
        let session_id = SessionKey("local:test".into());
        let task_id = octos_core::TaskId::new();
        let request = RpcRequest::new(
            "task-output-1",
            methods::TASK_OUTPUT_READ,
            json!({
                "session_id": session_id.clone(),
                "task_id": task_id.clone(),
                "cursor": { "offset": 4 },
                "limit_bytes": 16,
            }),
        );

        assert!(matches!(
            route_rpc_command(request).expect("task/output/read routes"),
            UiCommand::TaskOutputRead(params)
                if params.session_id == session_id
                    && params.task_id == task_id
                    && params.cursor.is_some_and(|cursor| cursor.offset == 4)
                    && params.limit_bytes == Some(16)
        ));
    }

    #[test]
    fn typed_approval_feature_is_negotiated_by_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            UI_FEATURES_HEADER,
            format!(
                "{UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1}, {UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1}"
            )
            .parse()
            .expect("header value"),
        );

        let features = ConnectionUiFeatures::from_headers_and_query(&headers, None);

        assert!(features.typed_approvals);
        assert!(features.pane_snapshots);
    }

    #[test]
    fn ui_features_can_be_negotiated_by_query_for_browser_websockets() {
        let headers = HeaderMap::new();
        let features = ConnectionUiFeatures::from_headers_and_query(
            &headers,
            Some("token=redacted&ui_feature=approval.typed.v1&ui_feature=pane.snapshots.v1"),
        );

        assert!(features.typed_approvals);
        assert!(features.pane_snapshots);
    }

    #[test]
    fn shell_approval_event_is_typed_only_after_negotiation() {
        let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
            tool_risk_registry_test_lock().clear_poison();
            e.into_inner()
        });
        clear_tool_risk_registry_for_test();
        register_tool_risk_for_test("shell", "medium");

        let request = ToolApprovalRequest {
            tool_id: "tool-1".into(),
            tool_name: "shell".into(),
            title: "Approve shell command".into(),
            body: "Command:\ncargo test".into(),
            command: Some("cargo test".into()),
            cwd: Some("/Users/yuechen/home/octos".into()),
        };
        let session_id = SessionKey("local:test".into());
        let approval_id = ApprovalId::new();
        let turn_id = TurnId::new();

        let generic = approval_event_from_tool_request(
            request.clone(),
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            ConnectionUiFeatures::default(),
        );
        assert!(generic.approval_kind.is_none());
        assert!(generic.typed_details.is_none());

        let typed = approval_event_from_tool_request(
            request,
            session_id,
            approval_id,
            turn_id,
            ConnectionUiFeatures {
                typed_approvals: true,
                pane_snapshots: false,
                session_workspace_cwd: false,
            },
        );
        assert_eq!(
            typed.approval_kind.as_deref(),
            Some(approval_kinds::COMMAND)
        );
        assert_eq!(typed.risk.as_deref(), Some("medium"));
        let command = typed
            .typed_details
            .as_ref()
            .and_then(|details| details.command.as_ref())
            .expect("typed command details");
        assert_eq!(command.command_line.as_deref(), Some("cargo test"));
        assert_eq!(command.cwd.as_deref(), Some("/Users/yuechen/home/octos"));
        assert_eq!(command.tool_call_id.as_deref(), Some("tool-1"));
        clear_tool_risk_registry_for_test();
    }

    #[test]
    fn risk_default_is_unspecified_when_manifest_silent() {
        let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
            tool_risk_registry_test_lock().clear_poison();
            e.into_inner()
        });
        clear_tool_risk_registry_for_test();

        let request = ToolApprovalRequest {
            tool_id: "tool-2".into(),
            tool_name: "shell".into(),
            title: "Approve shell command".into(),
            body: "Command:\nls".into(),
            command: Some("ls".into()),
            cwd: Some("/tmp".into()),
        };
        let event = approval_event_from_tool_request(
            request,
            SessionKey("local:test".into()),
            ApprovalId::new(),
            TurnId::new(),
            ConnectionUiFeatures {
                typed_approvals: true,
                pane_snapshots: false,
                session_workspace_cwd: false,
            },
        );

        assert_eq!(
            event.risk.as_deref(),
            Some(octos_core::ui_protocol::RISK_UNSPECIFIED),
            "manifest-silent tools must surface as `unspecified`, not `medium`"
        );
        clear_tool_risk_registry_for_test();
    }

    #[test]
    fn tool_emitted_risk_is_ignored_in_favor_of_manifest() {
        let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
            tool_risk_registry_test_lock().clear_poison();
            e.into_inner()
        });
        clear_tool_risk_registry_for_test();
        register_tool_risk_for_test("rm_rf", "critical");

        let mut tool_emitted = ApprovalRequestedEvent::generic(
            SessionKey("local:test".into()),
            ApprovalId::new(),
            TurnId::new(),
            "rm_rf",
            "Run destructive command",
            "/tmp/x",
        );
        // The malicious tool tries to advertise itself as `low`.
        tool_emitted.risk = Some("low".to_owned());

        harden_progress_emitted_approval(&mut tool_emitted);

        // Server overwrites with manifest-declared `critical`.
        assert_eq!(tool_emitted.risk.as_deref(), Some("critical"));

        // A tool whose manifest is silent collapses to `unspecified`,
        // never silently passes through the tool-claimed value.
        let mut silent = ApprovalRequestedEvent::generic(
            SessionKey("local:test".into()),
            ApprovalId::new(),
            TurnId::new(),
            "unknown_tool",
            "Unknown",
            "body",
        );
        silent.risk = Some("low".to_owned());
        harden_progress_emitted_approval(&mut silent);
        assert_eq!(
            silent.risk.as_deref(),
            Some(octos_core::ui_protocol::RISK_UNSPECIFIED)
        );
        clear_tool_risk_registry_for_test();
    }

    #[test]
    fn approval_cwd_is_sanitized_against_path_spoof() {
        let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
            tool_risk_registry_test_lock().clear_poison();
            e.into_inner()
        });
        clear_tool_risk_registry_for_test();
        let spoof_cwd = "/Users/safe\u{202E}gpj.exe/../../etc";
        let request = ToolApprovalRequest {
            tool_id: "tool-3".into(),
            tool_name: "shell".into(),
            title: "Approve shell command".into(),
            body: "Command:\nls".into(),
            command: Some("ls".into()),
            cwd: Some(spoof_cwd.into()),
        };
        let typed = approval_event_from_tool_request(
            request,
            SessionKey("local:test".into()),
            ApprovalId::new(),
            TurnId::new(),
            ConnectionUiFeatures {
                typed_approvals: true,
                pane_snapshots: false,
                session_workspace_cwd: false,
            },
        );
        let cwd = typed
            .typed_details
            .and_then(|details| details.command.and_then(|cmd| cmd.cwd))
            .expect("typed command cwd");
        assert!(!cwd.contains('\u{202E}'));
        assert!(!cwd.contains(".."));
        clear_tool_risk_registry_for_test();
    }

    #[test]
    fn task_output_delta_tracker_emits_live_tail_for_task_progress() {
        let session_id = SessionKey("local:test".into());
        let task_id = TaskId::new();
        let mut tracker = TaskOutputDeltaTracker::default();

        assert!(
            tracker
                .observe_progress_event(
                    &session_id,
                    &json!({ "type": "task_started", "task_id": task_id }),
                )
                .is_none()
        );

        let first = tracker
            .observe_progress_event(
                &session_id,
                &json!({ "type": "tool_progress", "message": "collecting\n" }),
            )
            .expect("progress message emits output delta");
        let second = tracker
            .observe_progress_event(
                &session_id,
                &json!({ "type": "task_output", "text": "done\n" }),
            )
            .expect("task output emits output delta");

        assert_eq!(first.session_id, session_id);
        assert_eq!(first.task_id, task_id);
        assert_eq!(first.cursor.offset, 0);
        assert_eq!(first.text, "collecting\n");
        assert_eq!(second.task_id, task_id);
        assert_eq!(second.cursor.offset, first.text.len() as u64);
        assert_eq!(second.text, "done\n");
    }

    #[test]
    fn task_output_delta_tracker_requires_task_identity() {
        let mut tracker = TaskOutputDeltaTracker::default();

        assert!(
            tracker
                .observe_progress_event(
                    &SessionKey("local:test".into()),
                    &json!({ "type": "tool_progress", "message": "running" }),
                )
                .is_none()
        );
    }

    #[test]
    fn approval_and_diff_commands_decode_protocol_params() {
        let session_id = SessionKey("local:test".into());
        let approval_id = ApprovalId::new();
        let approval = RpcRequest::new(
            "approval-1",
            methods::APPROVAL_RESPOND,
            json!({
                "session_id": session_id.clone(),
                "approval_id": approval_id.clone(),
                "decision": "approve",
            }),
        );

        assert!(matches!(
            route_rpc_command(approval).expect("approval/respond routes"),
            UiCommand::ApprovalRespond(ApprovalRespondParams {
                session_id: decoded_session_id,
                approval_id: decoded_approval_id,
                decision: ApprovalDecision::Approve,
                ..
            }) if decoded_session_id == session_id && decoded_approval_id == approval_id
        ));

        let preview_id = PreviewId::new();
        let diff = RpcRequest::new(
            "diff-1",
            methods::DIFF_PREVIEW_GET,
            json!({
                "session_id": session_id.clone(),
                "preview_id": preview_id.clone(),
            }),
        );

        assert!(matches!(
            route_rpc_command(diff).expect("diff/preview/get routes"),
            UiCommand::DiffPreviewGet(DiffPreviewGetParams {
                session_id: decoded_session_id,
                preview_id: decoded_preview_id,
            }) if decoded_session_id == session_id && decoded_preview_id == preview_id
        ));

        let task_id = TaskId::new();
        let task_cancel = RpcRequest::new(
            "task-cancel",
            methods::TASK_CANCEL,
            json!({
                "session_id": session_id.clone(),
                "task_id": task_id.clone(),
            }),
        );
        assert!(matches!(
            route_rpc_command(task_cancel).expect("task/cancel routes"),
            UiCommand::TaskCancel(TaskCancelParams {
                session_id: Some(decoded_session_id),
                task_id: decoded_task_id,
                ..
            }) if decoded_session_id == session_id && decoded_task_id == task_id
        ));
    }

    #[test]
    fn server_supported_methods_are_route_complete() {
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        let preview_id = PreviewId::new();
        let task_id = octos_core::TaskId::new();

        for request in [
            RpcRequest::new(
                "session-open",
                methods::SESSION_OPEN,
                json!({ "session_id": session_id.clone() }),
            ),
            RpcRequest::new(
                "turn-start",
                methods::TURN_START,
                json!({
                    "session_id": session_id.clone(),
                    "turn_id": turn_id.clone(),
                    "input": [{ "kind": "text", "text": "hello" }],
                }),
            ),
            RpcRequest::new(
                "turn-interrupt",
                methods::TURN_INTERRUPT,
                json!({
                    "session_id": session_id.clone(),
                    "turn_id": turn_id.clone(),
                }),
            ),
            RpcRequest::new(
                "approval-respond",
                methods::APPROVAL_RESPOND,
                json!({
                    "session_id": session_id.clone(),
                    "approval_id": approval_id.clone(),
                    "decision": "approve",
                }),
            ),
            RpcRequest::new(
                "diff-preview",
                methods::DIFF_PREVIEW_GET,
                json!({
                    "session_id": session_id.clone(),
                    "preview_id": preview_id.clone(),
                }),
            ),
            RpcRequest::new(
                "task-output",
                methods::TASK_OUTPUT_READ,
                json!({
                    "session_id": session_id.clone(),
                    "task_id": task_id.clone(),
                }),
            ),
            RpcRequest::new(
                "task-list",
                methods::TASK_LIST,
                json!({
                    "session_id": session_id.clone(),
                }),
            ),
            RpcRequest::new(
                "task-cancel",
                methods::TASK_CANCEL,
                json!({
                    "session_id": session_id.clone(),
                    "task_id": task_id.clone(),
                }),
            ),
            RpcRequest::new(
                "task-restart",
                methods::TASK_RESTART_FROM_NODE,
                json!({
                    "session_id": session_id.clone(),
                    "task_id": task_id.clone(),
                    "node_id": "design",
                }),
            ),
        ] {
            let method = request.method.clone();
            assert!(
                ui_protocol_server_supported_methods().contains(&method.as_str()),
                "{method} should be advertised by the server slice"
            );
            let command = route_rpc_command(request).expect("server-supported method routes");
            assert_eq!(command.method(), method);
        }
    }

    fn appui_task_state_with_running_task(
        session_id: &SessionKey,
    ) -> (Arc<AppState>, Arc<octos_agent::TaskSupervisor>, TaskId) {
        let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
        let task_id = supervisor.register(
            "run_pipeline",
            "call-appui-task",
            Some(&session_id.to_string()),
        );
        supervisor.mark_running(&task_id);
        let parsed_task_id = task_id
            .parse::<TaskId>()
            .expect("supervisor task id is UUID");

        let store = crate::session_actor::SessionTaskQueryStore::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        store.register(session_id, &supervisor, tmp.path());
        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        (state, supervisor, parsed_task_id)
    }

    async fn recv_rpc_json(rx: &mut mpsc::Receiver<WsMessage>) -> Value {
        match rx.recv().await.expect("rpc frame") {
            WsMessage::Text(text) => serde_json::from_str(text.as_str()).expect("json frame"),
            other => panic!("expected text frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn appui_task_list_returns_runtime_snapshot() {
        let session_id = SessionKey("local:test".into());
        let (state, _supervisor, task_id) = appui_task_state_with_running_task(&session_id);
        let (ws, mut rx) = ws_connection_for_test(8);

        handle_task_list(
            &ws,
            &state,
            None,
            "task-list-1".into(),
            TaskListParams {
                session_id: session_id.clone(),
                topic: None,
            },
        )
        .await;

        let frame = recv_rpc_json(&mut rx).await;
        assert_eq!(frame["id"], "task-list-1");
        assert_eq!(frame["result"]["session_id"], session_id.to_string());
        let tasks = frame["result"]["tasks"].as_array().expect("tasks array");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], task_id.to_string());
        assert_eq!(tasks[0]["status"], "running");
        assert_eq!(tasks[0]["state"], "running");
    }

    #[tokio::test]
    async fn appui_task_cancel_uses_supervisor_cancel_path() {
        let session_id = SessionKey("local:test".into());
        let (state, supervisor, task_id) = appui_task_state_with_running_task(&session_id);
        let (ws, mut rx) = ws_connection_for_test(8);

        handle_task_cancel(
            &ws,
            &state,
            None,
            "task-cancel-1".into(),
            TaskCancelParams {
                task_id: task_id.clone(),
                session_id: Some(session_id.clone()),
                profile_id: None,
            },
        )
        .await;

        let frame = recv_rpc_json(&mut rx).await;
        assert_eq!(frame["id"], "task-cancel-1");
        assert_eq!(frame["result"]["task_id"], task_id.to_string());
        assert_eq!(frame["result"]["status"], "cancelled");
        let task = supervisor
            .get_task(&task_id.to_string())
            .expect("task remains queryable");
        assert_eq!(task.status, octos_agent::TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn appui_task_restart_from_node_uses_relaunch_path() {
        let session_id = SessionKey("local:test".into());
        let (state, supervisor, task_id) = appui_task_state_with_running_task(&session_id);
        supervisor.mark_failed(&task_id.to_string(), "ready to relaunch".into());
        let (ws, mut rx) = ws_connection_for_test(8);

        handle_task_restart_from_node(
            &ws,
            &state,
            None,
            "task-restart-1".into(),
            TaskRestartFromNodeParams {
                task_id: task_id.clone(),
                node_id: Some("design".into()),
                session_id: Some(session_id),
                profile_id: None,
            },
        )
        .await;

        let frame = recv_rpc_json(&mut rx).await;
        assert_eq!(frame["id"], "task-restart-1");
        assert_eq!(frame["result"]["original_task_id"], task_id.to_string());
        assert_eq!(frame["result"]["from_node"], "design");
        let new_task_id = frame["result"]["new_task_id"]
            .as_str()
            .expect("new task id");
        assert_ne!(new_task_id, task_id.to_string());
        let successor = supervisor.get_task(new_task_id).expect("successor task");
        assert_eq!(successor.tool_name, "run_pipeline");
    }

    #[test]
    fn malformed_approval_params_return_invalid_params_not_unsupported() {
        // FIX-01 added `ApprovalDecision::Unknown(String)` — unknown decision
        // strings (e.g. `"later"`) are now valid forward-compat wire content
        // and decode to `Unknown(...)`. The server's downstream tool path
        // treats them as Deny (fail-closed). To trigger INVALID_PARAMS we
        // need *structurally* malformed params, e.g. `decision` of the wrong
        // JSON type.
        let request = RpcRequest::new(
            "approval-bad",
            methods::APPROVAL_RESPOND,
            json!({
                "session_id": "local:test",
                "approval_id": ApprovalId::new(),
                "decision": 42, // number where a string is required
            }),
        );

        let error = route_rpc_command(request).expect_err("bad params");

        assert_eq!(
            error.code,
            octos_core::ui_protocol::rpc_error_codes::INVALID_PARAMS
        );
        assert!(error.message.contains(methods::APPROVAL_RESPOND));
    }

    #[test]
    fn known_approval_returns_typed_json_rpc_result() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let approval_id = ApprovalId::new();
        contracts
            .approvals
            .insert_pending(session_id.clone(), approval_id.clone());

        let outcome = contracts
            .approvals
            .respond(ApprovalRespondParams::new(
                session_id,
                approval_id.clone(),
                ApprovalDecision::Approve,
            ))
            .expect("known pending approval accepts");
        let frame = RpcResponse::success(
            "approval-1",
            serde_json::to_value(outcome.result).expect("serialize result"),
        );

        assert_eq!(frame.jsonrpc, octos_core::ui_protocol::JSON_RPC_VERSION);
        assert_eq!(frame.id, "approval-1");
        assert_eq!(frame.result["approval_id"], json!(approval_id));
        assert_eq!(frame.result["accepted"], json!(true));
        assert_eq!(
            frame.result["status"],
            json!(ApprovalRespondStatus::Accepted)
        );
        assert_eq!(frame.result["runtime_resumed"], json!(false));
    }

    #[test]
    fn progress_approval_request_is_stored_for_respond() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let context = ProgressMappingContext::new(session_id.clone(), turn_id);
        let event = json!({
            "type": "approval_requested",
            "approval_id": ApprovalId::new(),
            "tool": "shell",
            "title": "Run command",
            "body": "cargo test",
        });
        let mut mapping = map_progress_json(&context, &event);

        apply_progress_contract_side_effects(&contracts, &context, None, &event, &mut mapping);

        let UiNotification::ApprovalRequested(request) = &mapping.notifications[0] else {
            panic!("expected approval/requested notification");
        };
        let outcome = contracts
            .approvals
            .respond(ApprovalRespondParams::new(
                session_id,
                request.approval_id.clone(),
                ApprovalDecision::Approve,
            ))
            .expect("produced approval can be responded to");

        assert!(outcome.result.accepted);
        assert!(!outcome.result.runtime_resumed);
    }

    #[test]
    fn missing_and_not_pending_approval_return_typed_json_rpc_errors() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let missing = contracts
            .approvals
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                ApprovalId::new(),
                ApprovalDecision::Approve,
            ))
            .expect_err("missing approval should fail");
        let frame = RpcErrorResponse::new(Some("approval-missing".into()), missing);

        assert_eq!(frame.jsonrpc, octos_core::ui_protocol::JSON_RPC_VERSION);
        assert_eq!(frame.id.as_deref(), Some("approval-missing"));
        assert_eq!(frame.error.code, rpc_error_codes::UNKNOWN_APPROVAL_ID);
        assert_eq!(
            frame.error.data.as_ref().unwrap()["kind"],
            json!("unknown_approval")
        );

        let approval_id = ApprovalId::new();
        contracts
            .approvals
            .insert_pending(session_id.clone(), approval_id.clone());
        contracts
            .approvals
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                approval_id.clone(),
                ApprovalDecision::Deny,
            ))
            .expect("first response accepts");
        let not_pending = contracts
            .approvals
            .respond(ApprovalRespondParams::new(
                session_id,
                approval_id,
                ApprovalDecision::Approve,
            ))
            .expect_err("second response should be not pending");

        assert_eq!(not_pending.code, rpc_error_codes::APPROVAL_NOT_PENDING);
        assert_eq!(
            not_pending.data.as_ref().unwrap()["kind"],
            json!("approval_not_pending")
        );
    }

    #[test]
    fn known_diff_preview_returns_typed_json_rpc_result() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let preview_id = PreviewId::new();
        contracts.diff_previews(None).insert(DiffPreview {
            session_id: session_id.clone(),
            preview_id: preview_id.clone(),
            title: Some("planned edit".into()),
            files: vec![DiffPreviewFile {
                path: "src/lib.rs".into(),
                old_path: None,
                status: DiffPreviewFileStatus::Modified,
                hunks: vec![DiffPreviewHunk {
                    header: "@@ -1 +1 @@".into(),
                    lines: vec![DiffPreviewLine {
                        kind: DiffPreviewLineKind::Added,
                        content: "let value = 1;".into(),
                        old_line: None,
                        new_line: Some(1),
                    }],
                }],
            }],
        });

        let result = contracts
            .diff_previews(None)
            .get(DiffPreviewGetParams {
                session_id,
                preview_id: preview_id.clone(),
            })
            .expect("known preview returns");
        let frame = RpcResponse::success(
            "diff-1",
            serde_json::to_value(result).expect("serialize result"),
        );

        assert_eq!(frame.result["status"], json!(DiffPreviewGetStatus::Ready));
        assert_eq!(
            frame.result["source"],
            json!(DiffPreviewSource::PendingStore)
        );
        assert_eq!(frame.result["preview"]["preview_id"], json!(preview_id));
        assert_eq!(
            frame.result["preview"]["files"][0]["path"],
            json!("src/lib.rs")
        );
    }

    #[test]
    fn progress_file_mutation_produces_gettable_diff_preview() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let context = ProgressMappingContext::new(session_id.clone(), TurnId::new());
        let event = json!({
            "type": "file_modified",
            "path": "src/lib.rs",
            "tool_call_id": "tool-1",
            "diff": "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
        });
        let mut mapping = map_progress_json(&context, &event);

        apply_progress_contract_side_effects(&contracts, &context, None, &event, &mut mapping);

        let preview_id = mapping
            .status
            .as_ref()
            .and_then(|status| status.event.metadata.file_mutation.as_ref())
            .and_then(|notice| notice.preview_id.clone())
            .expect("file mutation should expose a preview id");
        let result = contracts
            .diff_previews(None)
            .get(DiffPreviewGetParams {
                session_id,
                preview_id: preview_id.clone(),
            })
            .expect("produced preview should be readable");

        assert_eq!(result.preview.preview_id, preview_id);
        assert_eq!(result.preview.files[0].path, "src/lib.rs");
        assert_eq!(result.preview.files[0].hunks[0].lines[0].content, "old");
        assert_eq!(result.preview.files[0].hunks[0].lines[1].content, "new");
    }

    #[test]
    fn progress_file_mutation_materializes_git_diff_when_event_has_no_diff() {
        let repo = tempfile::tempdir().expect("temp repo");
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .arg("init")
                .status()
                .expect("git init")
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["config", "user.name", "octos-test"])
                .status()
                .expect("git config name")
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["config", "user.email", "octos-test@example.invalid"])
                .status()
                .expect("git config email")
                .success()
        );
        let src_dir = repo.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("src dir");
        let path = src_dir.join("lib.rs");
        std::fs::write(&path, "pub fn value() -> &'static str {\n    \"old\"\n}\n")
            .expect("write old");
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["add", "."])
                .status()
                .expect("git add")
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["commit", "-m", "initial"])
                .status()
                .expect("git commit")
                .success()
        );
        std::fs::write(&path, "pub fn value() -> &'static str {\n    \"new\"\n}\n")
            .expect("write new");

        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let context = ProgressMappingContext::new(session_id.clone(), TurnId::new());
        let event = json!({
            "type": "file_modified",
            "path": path,
            "tool_call_id": "tool-1",
        });
        let mut mapping = map_progress_json(&context, &event);

        apply_progress_contract_side_effects(&contracts, &context, None, &event, &mut mapping);

        let preview_id = mapping
            .status
            .as_ref()
            .and_then(|status| status.event.metadata.file_mutation.as_ref())
            .and_then(|notice| notice.preview_id.clone())
            .expect("file mutation should expose a preview id");
        let result = contracts
            .diff_previews(None)
            .get(DiffPreviewGetParams {
                session_id,
                preview_id,
            })
            .expect("produced preview should be readable");
        let lines = &result.preview.files[0].hunks[0].lines;

        assert!(lines.iter().any(|line| line.content.contains("\"old\"")));
        assert!(lines.iter().any(|line| line.content.contains("\"new\"")));
    }

    #[test]
    fn progress_file_mutation_materializes_relative_path_against_session_workspace() {
        let repo = tempfile::tempdir().expect("temp repo");
        for args in [
            vec!["init"],
            vec!["config", "user.name", "octos-test"],
            vec!["config", "user.email", "octos-test@example.invalid"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(repo.path())
                    .args(&args)
                    .status()
                    .expect("git command")
                    .success(),
                "git {args:?} setup failed"
            );
        }
        let src_dir = repo.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("src dir");
        let path = src_dir.join("lib.rs");
        std::fs::write(
            &path,
            "pub fn session_cwd() -> &'static str {\n    \"old\"\n}\n",
        )
        .expect("write old");
        for args in [vec!["add", "."], vec!["commit", "-m", "initial"]] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(repo.path())
                    .args(&args)
                    .status()
                    .expect("git command")
                    .success()
            );
        }
        std::fs::write(
            &path,
            "pub fn session_cwd() -> &'static str {\n    \"new\"\n}\n",
        )
        .expect("write new");

        assert_ne!(
            std::env::current_dir()
                .expect("process cwd")
                .canonicalize()
                .expect("canonical process cwd"),
            repo.path()
                .canonicalize()
                .expect("canonical session workspace")
        );

        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let context = ProgressMappingContext::new(session_id.clone(), TurnId::new());
        let event = json!({
            "type": "file_modified",
            "path": "src/lib.rs",
            "tool_call_id": "tool-1",
        });
        let mut mapping = map_progress_json(&context, &event);

        apply_progress_contract_side_effects(
            &contracts,
            &context,
            Some(repo.path()),
            &event,
            &mut mapping,
        );

        let preview_id = mapping
            .status
            .as_ref()
            .and_then(|status| status.event.metadata.file_mutation.as_ref())
            .and_then(|notice| notice.preview_id.clone())
            .expect("file mutation should expose a preview id");
        let result = contracts
            .diff_previews(None)
            .get(DiffPreviewGetParams {
                session_id,
                preview_id,
            })
            .expect("produced preview should be readable");
        let lines = &result.preview.files[0].hunks[0].lines;

        assert!(lines.iter().any(|line| line.content.contains("\"old\"")));
        assert!(lines.iter().any(|line| line.content.contains("\"new\"")));
    }

    #[test]
    fn materialize_file_mutation_diff_uses_snapshot_at_proposal_time() {
        // Sets up a real git repo, takes a proposal snapshot at t1, mutates
        // the file on disk at t2, and asserts that the cached preview at
        // t3 still reflects t1 — closing the proposal/apply TOCTOU on the
        // diff preview path.
        let repo = tempfile::tempdir().expect("temp repo");
        for args in [
            vec!["init"],
            vec!["config", "user.name", "octos-test"],
            vec!["config", "user.email", "octos-test@example.invalid"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(repo.path())
                    .args(&args)
                    .status()
                    .expect("git command")
                    .success(),
                "git {args:?} setup failed"
            );
        }
        let src_dir = repo.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("src dir");
        let path = src_dir.join("lib.rs");
        std::fs::write(&path, "pub fn value() -> &'static str {\n    \"v0\"\n}\n")
            .expect("write v0");
        for args in [vec!["add", "."], vec!["commit", "-m", "v0"]] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(repo.path())
                    .args(&args)
                    .status()
                    .expect("git command")
                    .success()
            );
        }

        // t1 — propose: working-tree has v1, runtime emits the progress event.
        std::fs::write(&path, "pub fn value() -> &'static str {\n    \"v1\"\n}\n")
            .expect("write v1");

        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let context = ProgressMappingContext::new(session_id.clone(), TurnId::new());
        let event = json!({
            "type": "file_modified",
            "path": path,
            "tool_call_id": "tool-1",
        });
        let mut mapping = map_progress_json(&context, &event);
        apply_progress_contract_side_effects(&contracts, &context, None, &event, &mut mapping);

        let preview_id = mapping
            .status
            .as_ref()
            .and_then(|status| status.event.metadata.file_mutation.as_ref())
            .and_then(|notice| notice.preview_id.clone())
            .expect("file mutation should expose a preview id");

        // t2 — concurrent writer rewrites the file on disk to v2.
        std::fs::write(&path, "pub fn value() -> &'static str {\n    \"v2\"\n}\n")
            .expect("write v2");

        // t3 — fetch the cached preview. It must still reflect v1 (the
        // proposal-time snapshot), not v2 (the current FS).
        let result = contracts
            .diff_previews(None)
            .get(DiffPreviewGetParams {
                session_id: session_id.clone(),
                preview_id: preview_id.clone(),
            })
            .expect("preview should still be readable post-mutation");
        let lines = &result.preview.files[0].hunks[0].lines;
        assert!(
            lines.iter().any(|line| line.content.contains("\"v1\"")),
            "snapshot must include v1 added line"
        );
        assert!(
            !lines.iter().any(|line| line.content.contains("\"v2\"")),
            "post-proposal mutation must not leak into the cached preview"
        );

        // The raw diff bytes captured at proposal time are also preserved
        // for downstream apply-time consistency checks.
        let snapshot = contracts
            .diff_previews(None)
            .snapshot_for(&preview_id)
            .expect("snapshot should be retained for the entry");
        assert!(snapshot.contains("\"v1\""));
        assert!(!snapshot.contains("\"v2\""));
    }

    #[test]
    fn missing_diff_preview_returns_typed_json_rpc_error() {
        let contracts = UiProtocolContractStores::default();
        let missing = contracts
            .diff_previews(None)
            .get(DiffPreviewGetParams {
                session_id: SessionKey("local:test".into()),
                preview_id: PreviewId::new(),
            })
            .expect_err("missing preview should fail");
        let frame = RpcErrorResponse::new(Some("diff-missing".into()), missing);

        assert_eq!(frame.id.as_deref(), Some("diff-missing"));
        assert_eq!(frame.error.code, rpc_error_codes::UNKNOWN_PREVIEW_ID);
        assert_eq!(
            frame.error.data.as_ref().unwrap()["kind"],
            json!("unknown_preview")
        );
    }

    #[test]
    fn rejects_invalid_rpc_request_json() {
        let error = parse_rpc_request("{").expect_err("parse error");
        assert_eq!(
            error.code,
            octos_core::ui_protocol::rpc_error_codes::PARSE_ERROR
        );
    }

    #[test]
    fn oversized_text_frame_is_rejected_before_json_parse() {
        let text = "x".repeat(MAX_TEXT_FRAME_BYTES + 1);

        let error = parse_ws_text_frame(&text).expect_err("oversized frame");

        assert_eq!(error.code, FRAME_TOO_LARGE);
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("limit_bytes")),
            Some(&json!(MAX_TEXT_FRAME_BYTES))
        );
    }

    #[test]
    fn authenticated_profile_id_uses_user_identity_only() {
        let user = AuthIdentity::User {
            id: "profile-a".into(),
            role: UserRole::User,
        };

        assert_eq!(authenticated_profile_id(&user), Some("profile-a"));
        assert_eq!(authenticated_profile_id(&AuthIdentity::Admin), None);
    }

    #[test]
    fn session_scope_allows_matching_authenticated_profile() {
        let session_id = SessionKey::with_profile("profile-a", "api", "chat-1");

        let active_profile_id =
            validate_session_scope(&session_id, Some("profile-a"), Some("profile-a"))
                .expect("valid scope");

        assert_eq!(active_profile_id.as_deref(), Some("profile-a"));
    }

    #[test]
    fn session_scope_rejects_cross_profile_session_id() {
        let session_id = SessionKey::with_profile("profile-b", "api", "chat-1");

        let error =
            validate_session_scope(&session_id, None, Some("profile-a")).expect_err("scope error");

        assert_eq!(
            error.code,
            octos_core::ui_protocol::rpc_error_codes::INVALID_PARAMS
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("expected_profile_id")),
            Some(&Value::String("profile-a".into()))
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("actual_profile_id")),
            Some(&Value::String("profile-b".into()))
        );
    }

    #[test]
    fn session_scope_rejects_unprofiled_session_id_when_authenticated() {
        let session_id = SessionKey::new("api", "chat-1");

        let error =
            validate_session_scope(&session_id, None, Some("profile-a")).expect_err("scope error");

        assert_eq!(
            error.code,
            octos_core::ui_protocol::rpc_error_codes::INVALID_PARAMS
        );
        assert!(error.message.contains("authenticated profile"));
    }

    #[test]
    fn session_scope_rejects_cross_profile_open_param() {
        let session_id = SessionKey::with_profile("profile-a", "api", "chat-1");

        let error = validate_session_scope(&session_id, Some("profile-b"), Some("profile-a"))
            .expect_err("scope error");

        assert_eq!(
            error.code,
            octos_core::ui_protocol::rpc_error_codes::INVALID_PARAMS
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("actual_profile_id")),
            Some(&Value::String("profile-b".into()))
        );
    }

    #[test]
    fn session_scope_preserves_legacy_keys_without_profile_context() {
        let legacy_session_id = SessionKey::new("api", "chat-1");
        let profiled_session_id = SessionKey::with_profile("profile-a", "api", "chat-1");

        assert_eq!(
            validate_session_scope(&legacy_session_id, None, None).expect("legacy scope"),
            None
        );
        assert_eq!(
            validate_session_scope(&profiled_session_id, None, None)
                .expect("profiled scope")
                .as_deref(),
            Some("profile-a")
        );
    }

    #[test]
    fn prompt_text_requires_non_empty_text_input() {
        assert_eq!(
            prompt_text(&[InputItem::Text {
                text: "hello".into()
            }]),
            Some("hello".into())
        );
        assert_eq!(
            prompt_text(&[
                InputItem::Text { text: "a".into() },
                InputItem::Text { text: "b".into() }
            ]),
            Some("a\nb".into())
        );
        assert_eq!(prompt_text(&[InputItem::Text { text: "   ".into() }]), None);
    }

    fn state_with_sessions(data_dir: &std::path::Path) -> Arc<AppState> {
        Arc::new(AppState {
            sessions: Some(Arc::new(tokio::sync::Mutex::new(
                octos_bus::SessionManager::open(data_dir).expect("session manager"),
            ))),
            ..AppState::empty_for_tests()
        })
    }

    /// Build an `ActiveTurn` with default `Active` state for tests that drive
    /// the registry directly without going through `handle_turn_start`.
    fn test_active_turn(turn_id: TurnId, abort: AbortHandle) -> ActiveTurn {
        let (tx, _rx) = mpsc::channel::<()>(1);
        ActiveTurn {
            turn_id,
            state: Arc::new(TokioMutex::new(TurnState::Active)),
            interrupt_tx: Arc::new(TokioMutex::new(Some(tx))),
            abort,
        }
    }

    #[tokio::test]
    async fn session_open_replays_notifications_after_cursor_and_returns_ledger_cursor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let first = ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            text: "one".into(),
        }));
        ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session_id.clone(),
            turn_id,
            text: "two".into(),
        }));

        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: None,
                cwd: None,
                after: Some(first.cursor),
            },
        )
        .await
        .expect("open session after retained cursor");

        assert_eq!(outcome.result.opened.session_id, session_id);
        assert_eq!(outcome.result.opened.cursor.expect("cursor").seq, 3);
        assert_eq!(outcome.replay.len(), 1);
        assert_eq!(outcome.replay[0].cursor.seq, 2);
        assert!(matches!(
            &outcome.replay[0].event,
            UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(event))
                if event.text == "two"
        ));
    }

    #[tokio::test]
    async fn session_open_rejects_after_cursor_from_other_stream() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());

        let error = open_session_result(
            &state,
            &ledger,
            &approvals,
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: None,
                cwd: None,
                after: Some(UiCursor {
                    stream: "local:other".into(),
                    seq: 0,
                }),
            },
        )
        .await
        .expect_err("foreign stream cursor should fail");

        assert_eq!(
            error.code,
            octos_core::ui_protocol::rpc_error_codes::CURSOR_INVALID
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("cursor_stream_mismatch"))
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("expected_stream")),
            Some(&json!(session_id.0))
        );
    }

    #[tokio::test]
    async fn session_open_rejects_stale_after_cursor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(1);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            text: "one".into(),
        }));
        ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session_id.clone(),
            turn_id,
            text: "two".into(),
        }));

        let error = open_session_result(
            &state,
            &ledger,
            &approvals,
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: None,
                cwd: None,
                after: Some(UiCursor {
                    stream: session_id.0.clone(),
                    seq: 0,
                }),
            },
        )
        .await
        .expect_err("stale cursor should fail");

        assert_eq!(
            error.code,
            octos_core::ui_protocol::rpc_error_codes::CURSOR_OUT_OF_RANGE
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("cursor_expired"))
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("oldest_retained_seq")),
            Some(&json!(2))
        );
    }

    #[tokio::test]
    async fn session_open_replays_pending_approval_after_reconnect_without_cursor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let approval_id = ApprovalId::new();
        approvals.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            TurnId::new(),
            "shell",
            "Run command",
            "cargo test",
        ));

        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: None,
                cwd: None,
                after: None,
            },
        )
        .await
        .expect("open session should replay pending approval");

        assert!(outcome.replay.is_empty());
        assert_eq!(outcome.pending_approvals.len(), 1);
        assert_eq!(outcome.pending_approvals[0].approval_id, approval_id);
        assert_eq!(outcome.pending_approvals[0].title, "Run command");
    }

    #[tokio::test]
    async fn session_open_does_not_duplicate_pending_approval_already_in_cursor_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let approval_id = ApprovalId::new();
        let approval = ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            TurnId::new(),
            "shell",
            "Run command",
            "cargo test",
        );
        approvals.request(approval.clone());
        ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session_id.clone(),
            turn_id: TurnId::new(),
            text: "before".into(),
        }));
        ledger.append_notification(UiNotification::ApprovalRequested(approval));

        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: None,
                cwd: None,
                after: Some(UiCursor {
                    stream: session_id.0.clone(),
                    seq: 1,
                }),
            },
        )
        .await
        .expect("open session should rely on cursor replay");

        assert_eq!(outcome.replay.len(), 1);
        assert!(matches!(
            &outcome.replay[0].event,
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(event))
                if event.approval_id == approval_id
        ));
        assert!(outcome.pending_approvals.is_empty());
    }

    #[tokio::test]
    async fn session_open_includes_pane_snapshot_after_negotiation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let workspace = ui_protocol_session_workspace_dirs(temp.path(), &session_id, None)
            .into_iter()
            .next()
            .expect("workspace candidate");
        std::fs::create_dir_all(workspace.join("src")).expect("create workspace");
        std::fs::write(workspace.join("src").join("lib.rs"), "pub fn pane() {}\n")
            .expect("write workspace file");

        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            None,
            ConnectionUiFeatures {
                typed_approvals: false,
                pane_snapshots: true,
                session_workspace_cwd: false,
            },
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: None,
                cwd: None,
                after: None,
            },
        )
        .await
        .expect("open session with pane snapshots");

        let panes = outcome
            .result
            .opened
            .panes
            .expect("pane snapshots negotiated");
        let workspace = panes.workspace.expect("workspace pane");
        assert!(workspace.entries.iter().any(|entry| {
            entry.path == "src/lib.rs" && entry.kind == "file" && entry.detail.is_some()
        }));
        let artifacts = panes.artifacts.expect("artifact pane");
        assert!(
            artifacts
                .items
                .iter()
                .any(|item| item.title == "lib.rs" && item.path.as_deref() == Some("src/lib.rs"))
        );
        let git = panes.git.expect("git pane");
        assert!(
            git.limitations
                .iter()
                .any(|limitation| limitation.code == "git_unavailable")
        );
    }

    #[tokio::test]
    async fn session_open_rejects_cwd_without_negotiated_feature() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:cwd-feature".into());

        let error = open_session_result(
            &state,
            &ledger,
            &approvals,
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_id,
                profile_id: None,
                cwd: Some(temp.path().to_string_lossy().to_string()),
                after: None,
            },
        )
        .await
        .expect_err("cwd should require negotiated feature");

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("feature_required"))
        );
    }

    #[test]
    fn session_workspace_authorizes_approved_subdir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let tools = octos_agent::ToolRegistry::with_builtins(temp.path());

        session_filesystem_profile_for_workspace(&tools, &project).expect("subdir is approved");
    }

    #[test]
    fn session_workspace_rejects_outside_root() {
        let allowed = tempfile::tempdir().expect("allowed dir");
        let outside = tempfile::tempdir().expect("outside dir");
        let tools = octos_agent::ToolRegistry::with_builtins(allowed.path());

        let error = session_filesystem_profile_for_workspace(&tools, outside.path())
            .expect_err("outside workspace should be rejected");

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("cwd_outside_workspace_root"))
        );
    }

    /// Minimal stub LLM provider for tests that build an `Agent` but never
    /// actually call out to a model. `session_tool_registry` only inspects
    /// the agent's tool registry and sandbox config — it never drives the
    /// LLM — so a `chat` panic guard is enough.
    struct StubLlm;

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for StubLlm {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            unreachable!("StubLlm should not be invoked by session_tool_registry tests")
        }

        fn context_window(&self) -> u32 {
            128_000
        }

        fn model_id(&self) -> &str {
            "stub"
        }

        fn provider_name(&self) -> &str {
            "stub"
        }
    }

    /// Tier-2 of the AppUi `session_tool_registry` fallback chain: when a
    /// session has no entry in `session_workspaces()` and the operator
    /// configured `appui.default_session_cwd`, the API agent's recorded
    /// `workspace_root` is used. This locks down the behavior added by
    /// `Agent::with_workspace_root` + `serve.rs` wire-up.
    #[tokio::test]
    async fn session_tool_registry_uses_agent_workspace_root_as_tier2_fallback() {
        let workspace = tempfile::tempdir().expect("workspace dir");
        let memory_dir = tempfile::tempdir().expect("memory dir");
        let memory = Arc::new(
            octos_memory::EpisodeStore::open(memory_dir.path())
                .await
                .expect("open memory"),
        );
        let llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(StubLlm);
        let tools = octos_agent::ToolRegistry::with_builtins(workspace.path());

        let agent = octos_agent::Agent::new(AgentId::new("api-test"), llm, tools, memory)
            .with_workspace_root(workspace.path().to_path_buf());

        // Use a unique session_id so we don't collide with other tests on
        // the process-global `session_workspaces()` store.
        let session_id = SessionKey("local:tier2-fallback-test".into());

        let (registry, root) =
            session_tool_registry(&agent, &session_id).expect("session_tool_registry");

        let root = root.expect("Tier-2 must populate workspace_root");
        assert_eq!(
            std::fs::canonicalize(&root).expect("canonicalize"),
            std::fs::canonicalize(workspace.path()).expect("canonicalize"),
            "Tier-2 fallback should pick up the agent's recorded workspace_root"
        );
        assert!(
            registry.workspace_root().is_some(),
            "rebound registry must record the workspace_root"
        );
    }

    /// Tier-1 (capability-gated client-sent cwd via `session_workspaces`)
    /// MUST take precedence over Tier-2 (operator default). This prevents
    /// the operator default from clobbering octos-tui's per-session picker.
    ///
    /// `session_filesystem_profile_for_workspace` requires the requested
    /// cwd to live under `tools.workspace_root()`. Since
    /// `with_workspace_root` overwrites the registry's recorded root, we
    /// anchor the agent at the operator-default folder (Tier-2) and put
    /// the client-sent cwd as a subdir of it. If Tier-1 wins, the
    /// resulting workspace_root is the subdir, not the parent.
    #[tokio::test]
    async fn session_tool_registry_tier1_wins_over_tier2_default() {
        let tier2_default = tempfile::tempdir().expect("tier2 default dir");
        let tier1_subdir = tier2_default.path().join("tier1-client-cwd");
        std::fs::create_dir_all(&tier1_subdir).expect("tier1 subdir");

        let memory_dir = tempfile::tempdir().expect("memory dir");
        let memory = Arc::new(
            octos_memory::EpisodeStore::open(memory_dir.path())
                .await
                .expect("open memory"),
        );
        let llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(StubLlm);
        let tools = octos_agent::ToolRegistry::with_builtins(tier2_default.path());

        let agent = octos_agent::Agent::new(AgentId::new("api-test"), llm, tools, memory)
            // Tier-2: operator-configured default cwd.
            .with_workspace_root(tier2_default.path().to_path_buf());

        // Tier-1: client-sent cwd recorded in `session_workspaces`.
        let session_id = SessionKey("local:tier1-wins-test".into());
        session_workspaces().set(session_id.clone(), tier1_subdir.clone());

        let (_registry, root) =
            session_tool_registry(&agent, &session_id).expect("session_tool_registry");

        let root = root.expect("workspace_root must be set");
        assert_eq!(
            std::fs::canonicalize(&root).expect("canonicalize"),
            std::fs::canonicalize(&tier1_subdir).expect("canonicalize"),
            "Tier-1 (client-sent cwd) must win over Tier-2 (operator default)"
        );
    }

    #[test]
    fn pane_snapshot_prefers_approved_session_workspace_root() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let project = tempfile::tempdir().expect("project dir");
        let src = project.path().join("src");
        std::fs::create_dir_all(&src).expect("src dir");
        std::fs::write(src.join("main.rs"), "fn main() {}\n").expect("write file");
        let session_id = SessionKey("local:cwd-pane".into());

        let panes = build_pane_snapshot(data_dir.path(), &session_id, Some(project.path()));
        let workspace = panes.workspace.expect("workspace pane");

        assert_eq!(workspace.root, project.path().to_string_lossy());
        assert!(workspace.entries.iter().any(|entry| {
            entry.path == "src/main.rs" && entry.kind == "file" && entry.detail.is_some()
        }));
    }

    #[test]
    fn runtime_unavailable_errors_are_typed_for_protocol_clients() {
        let error = runtime_unavailable_error("No LLM provider configured");

        assert_eq!(
            error.code,
            octos_core::ui_protocol::rpc_error_codes::INTERNAL_ERROR
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("runtime_unavailable"))
        );
    }

    #[test]
    fn final_assistant_message_persists_content_when_response_messages_omit_it() {
        let message = final_assistant_message(&[Message::user("hello")], "world", Some("r".into()))
            .expect("assistant message");

        assert_eq!(message.role, MessageRole::Assistant);
        assert_eq!(message.content, "world");
        assert_eq!(message.reasoning_content.as_deref(), Some("r"));
    }

    #[test]
    fn final_assistant_message_skips_duplicate_assistant_content() {
        let messages = vec![Message::assistant("world")];

        assert!(final_assistant_message(&messages, "world", None).is_none());
    }

    #[tokio::test]
    async fn abort_connection_turns_removes_only_matching_active_turns() {
        let owned_session_id = SessionKey("local:owned".into());
        let stale_session_id = SessionKey("local:stale".into());
        let owned_turn_id = TurnId::new();
        let stale_connection_turn_id = TurnId::new();
        let newer_turn_id = TurnId::new();
        let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let connection_turns: SharedConnectionTurns =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let owned_handle = tokio::spawn(async { std::future::pending::<()>().await });
        let newer_handle = tokio::spawn(async { std::future::pending::<()>().await });
        active_turns.lock().await.insert(
            owned_session_id.clone(),
            test_active_turn(owned_turn_id.clone(), owned_handle.abort_handle()),
        );
        active_turns.lock().await.insert(
            stale_session_id.clone(),
            test_active_turn(newer_turn_id.clone(), newer_handle.abort_handle()),
        );
        connection_turns
            .lock()
            .await
            .insert(owned_session_id.clone(), owned_turn_id);
        connection_turns
            .lock()
            .await
            .insert(stale_session_id.clone(), stale_connection_turn_id);

        let scopes = ScopePolicy::default();
        abort_connection_turns(&active_turns, &connection_turns, &scopes).await;

        assert!(!active_turns.lock().await.contains_key(&owned_session_id));
        assert_eq!(
            active_turns
                .lock()
                .await
                .get(&stale_session_id)
                .map(|active| active.turn_id.clone()),
            Some(newer_turn_id)
        );
        assert!(connection_turns.lock().await.is_empty());
        owned_handle.abort();
        newer_handle.abort();
    }

    /// Mirror of `handle_turn_interrupt`'s post-abort drain step. Used by
    /// the interrupt-flow tests below to drive the store + ledger without
    /// constructing a real `WsSink`.
    fn drain_pending_approvals_for_interrupt(
        ledger: &UiProtocolLedger,
        approvals: &PendingApprovalStore,
        session_id: &SessionKey,
        turn_id: &TurnId,
    ) -> Vec<ApprovalCancelledEvent> {
        let cancelled = approvals.cancel_pending_for_turn(
            session_id,
            turn_id,
            approval_cancelled_reasons::TURN_INTERRUPTED,
        );
        let mut emitted = Vec::with_capacity(cancelled.len());
        for entry in cancelled {
            let event = ApprovalCancelledEvent::turn_interrupted(
                session_id.clone(),
                entry.approval_id,
                entry.turn_id,
            );
            ledger.append_notification(UiNotification::ApprovalCancelled(event.clone()));
            emitted.push(event);
        }
        emitted
    }

    #[tokio::test]
    async fn interrupt_cancels_pending_approvals_for_turn() {
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let interrupted_turn = TurnId::new();
        let approval_id = ApprovalId::new();
        let surviving_turn = TurnId::new();
        let surviving_approval = ApprovalId::new();

        approvals.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            interrupted_turn.clone(),
            "shell",
            "Pending",
            "ls",
        ));
        approvals.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            surviving_approval.clone(),
            surviving_turn,
            "shell",
            "Different turn",
            "ls",
        ));

        let emitted = drain_pending_approvals_for_interrupt(
            &ledger,
            &approvals,
            &session_id,
            &interrupted_turn,
        );

        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].approval_id, approval_id);
        assert_eq!(emitted[0].turn_id, interrupted_turn);
        assert_eq!(emitted[0].reason, "turn_interrupted");

        let err = approvals
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                approval_id,
                ApprovalDecision::Approve,
            ))
            .expect_err("late respond against cancelled approval");
        assert_eq!(err.code, rpc_error_codes::APPROVAL_CANCELLED);

        // Approval on the surviving (non-interrupted) turn still works.
        let ok = approvals
            .respond(ApprovalRespondParams::new(
                session_id,
                surviving_approval,
                ApprovalDecision::Approve,
            ))
            .expect("non-interrupted turn approval still pending");
        // FIX-06 wrapped the result in `RespondOutcome { result, context }`.
        assert!(ok.result.accepted);
    }

    #[tokio::test]
    async fn interrupt_with_no_pending_approvals_is_no_op() {
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();

        let first =
            drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &turn_id);
        let second =
            drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &turn_id);

        assert!(first.is_empty(), "no approvals to cancel on first call");
        assert!(second.is_empty(), "double-interrupt is idempotent");
    }

    #[tokio::test]
    async fn cancelled_approval_replays_on_reconnect() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        let approval = ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Pending",
            "ls",
        );
        approvals.request(approval.clone());
        // The original approval/requested notification is in the durable
        // ledger (typical lifecycle when M9-FIX-01 is active).
        ledger.append_notification(UiNotification::ApprovalRequested(approval));

        let emitted =
            drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &turn_id);
        assert_eq!(emitted.len(), 1);

        // A reconnecting client with no cursor must rebuild from the ledger
        // replay; pending_for_session must NOT yield the cancelled approval
        // (otherwise the UI would re-render a fresh card after seeing the
        // cancellation event).
        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: None,
                cwd: None,
                after: None,
            },
        )
        .await
        .expect("session/open after cancellation");
        assert!(
            outcome.pending_approvals.is_empty(),
            "cancelled approvals must not surface as fresh pending replays",
        );

        // A reconnecting client *with* a pre-cancellation cursor must see
        // the durable approval/cancelled event in the cursor-bounded replay.
        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: None,
                cwd: None,
                after: Some(UiCursor {
                    stream: session_id.0.clone(),
                    seq: 0,
                }),
            },
        )
        .await
        .expect("session/open with cursor 0 replays everything");
        assert!(outcome.replay.iter().any(|event| matches!(
            &event.event,
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalCancelled(event))
                if event.approval_id == approval_id
                    && event.reason == "turn_interrupted"
        )));
    }

    #[tokio::test]
    async fn respond_to_cancelled_approval_returns_typed_error() {
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        approvals.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Pending",
            "ls",
        ));

        drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &turn_id);

        let err = approvals
            .respond(ApprovalRespondParams::new(
                session_id,
                approval_id.clone(),
                ApprovalDecision::Approve,
            ))
            .expect_err("late respond returns typed error");
        assert_eq!(err.code, rpc_error_codes::APPROVAL_CANCELLED);
        let data = err.data.expect("typed error data");
        assert_eq!(data["kind"], json!("approval_cancelled"));
        assert_eq!(data["reason"], json!("turn_interrupted"));
        assert_eq!(data["approval_id"], json!(approval_id));
    }

    #[tokio::test]
    async fn one_hundred_concurrent_interrupts_emit_cancellation_exactly_once() {
        // Stress: even with 100 racing interrupts on the same session/turn,
        // the cancellation transition is exactly-once and emits one
        // approval/cancelled per pending approval.
        let ledger = Arc::new(UiProtocolLedger::new(2048));
        let approvals = Arc::new(PendingApprovalStore::default());
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_count = 8usize;
        let mut approval_ids = Vec::with_capacity(approval_count);
        for _ in 0..approval_count {
            let approval_id = ApprovalId::new();
            approvals.request(ApprovalRequestedEvent::generic(
                session_id.clone(),
                approval_id.clone(),
                turn_id.clone(),
                "shell",
                "Pending",
                "ls",
            ));
            approval_ids.push(approval_id);
        }

        let mut handles = Vec::with_capacity(100);
        for _ in 0..100 {
            let approvals = Arc::clone(&approvals);
            let session_id = session_id.clone();
            let turn_id = turn_id.clone();
            handles.push(tokio::spawn(async move {
                approvals.cancel_pending_for_turn(
                    &session_id,
                    &turn_id,
                    approval_cancelled_reasons::TURN_INTERRUPTED,
                )
            }));
        }

        let mut total_cancelled = 0usize;
        let mut seen_ids = HashSet::new();
        for handle in handles {
            let cancelled = handle.await.expect("interrupt task");
            for entry in cancelled {
                assert!(
                    seen_ids.insert(entry.approval_id.clone()),
                    "double-emit detected for {:?}",
                    entry.approval_id,
                );
                total_cancelled += 1;
            }
        }

        assert_eq!(
            total_cancelled, approval_count,
            "exactly one cancellation per pending approval across 100 racing interrupts",
        );
        for approval_id in &approval_ids {
            let err = approvals
                .respond(ApprovalRespondParams::new(
                    session_id.clone(),
                    approval_id.clone(),
                    ApprovalDecision::Approve,
                ))
                .expect_err("respond against cancelled approval fails");
            assert_eq!(err.code, rpc_error_codes::APPROVAL_CANCELLED);
        }

        // We never emitted notifications above because the test exercises
        // the store directly; the ledger must therefore be empty for this
        // session.
        assert!(
            ledger
                .replay_after(
                    &session_id,
                    Some(&UiCursor {
                        stream: session_id.0.clone(),
                        seq: 0,
                    }),
                )
                .expect("replay")
                .is_empty(),
            "stress test should not write to the ledger",
        );
    }

    // TODO(M9-FIX-06): once ScopePolicy lands in this worktree, add a test
    // verifying that approve_for_session scopes survive turn/interrupt while
    // approve_for_turn and per-call pending entries are cancelled. The
    // supervisor will reconcile the test during merge.

    #[test]
    fn notification_serializes_as_json_rpc_method_frame() {
        let frame = UiNotification::TurnError(TurnErrorEvent {
            session_id: SessionKey("local:test".into()),
            turn_id: TurnId::new(),
            code: "test".into(),
            message: "failed".into(),
        })
        .into_rpc_notification()
        .expect("notification");

        assert_eq!(frame.method, methods::TURN_ERROR);
    }

    // ====================================================================
    // M9-FIX-03 — interrupt/turn state-machine + TOCTOU repro
    // ====================================================================

    /// Insert an `ActiveTurn` whose state has already moved to `Terminal(_)`
    /// — emulates the world after natural completion of a prior turn.
    async fn insert_terminal_turn(
        active_turns: &SharedActiveTurns,
        session_id: &SessionKey,
        turn_id: &TurnId,
        reason: TerminalReason,
    ) -> tokio::task::JoinHandle<()> {
        let handle = tokio::spawn(async { std::future::pending::<()>().await });
        let entry = test_active_turn(turn_id.clone(), handle.abort_handle());
        *entry.state.lock().await = TurnState::Terminal(reason);
        active_turns.lock().await.insert(session_id.clone(), entry);
        handle
    }

    #[tokio::test]
    async fn interrupt_idempotent_on_completed_turn() {
        let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let handle = insert_terminal_turn(
            &active_turns,
            &session_id,
            &turn_id,
            TerminalReason::Completed,
        )
        .await;

        let outcome = decide_interrupt(
            &active_turns,
            &TurnInterruptParams {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            },
        )
        .await;

        assert!(matches!(
            outcome,
            InterruptOutcome::AlreadyTerminal(TerminalReason::Completed)
        ));
        // A second interrupt returns the same shape — idempotent.
        let outcome2 = decide_interrupt(
            &active_turns,
            &TurnInterruptParams {
                session_id,
                turn_id,
            },
        )
        .await;
        assert!(matches!(
            outcome2,
            InterruptOutcome::AlreadyTerminal(TerminalReason::Completed)
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn interrupt_unknown_turn_returns_unknown_turn_error() {
        let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let turn_id = TurnId::new();

        let outcome = decide_interrupt(
            &active_turns,
            &TurnInterruptParams {
                session_id: SessionKey("local:test".into()),
                turn_id: turn_id.clone(),
            },
        )
        .await;
        assert!(matches!(outcome, InterruptOutcome::Unknown));

        let error = unknown_turn_error(&turn_id);
        assert_eq!(error.code, UNKNOWN_TURN_CODE);
        assert_eq!(
            error.data.as_ref().and_then(|d| d.get("kind")),
            Some(&json!("unknown_turn"))
        );
    }

    #[tokio::test]
    async fn interrupt_in_flight_turn_aborts_emits_one_terminal() {
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let handle = tokio::spawn(async { std::future::pending::<()>().await });
        let entry = test_active_turn(turn_id.clone(), handle.abort_handle());
        let turn_state = entry.state.clone();
        active_turns.lock().await.insert(session_id.clone(), entry);

        let outcome = decide_interrupt(
            &active_turns,
            &TurnInterruptParams {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            },
        )
        .await;
        let ack_rx = match outcome {
            InterruptOutcome::Captured { ack_rx } => ack_rx,
            other => panic!("expected Captured, got {other:?}"),
        };
        assert!(matches!(
            *turn_state.lock().await,
            TurnState::Interrupting { .. }
        ));

        // Simulate the turn task winning by transitioning Interrupting →
        // Terminal(Interrupted) and signalling ack. The `expected` reason
        // (Completed) is overridden because state is `Interrupting`.
        let transition = transition_to_terminal(&turn_state, TerminalReason::Completed)
            .await
            .expect("first transition wins");
        assert_eq!(transition.reason, TerminalReason::Interrupted);
        if let Some(ack) = transition.ack {
            ack.send(()).expect("ack delivered");
        }
        assert_eq!(ack_rx.await.expect("handler observes ack"), ());

        // A second transition must be a no-op — no double-emit possible.
        let second = transition_to_terminal(&turn_state, TerminalReason::Errored).await;
        assert!(second.is_none(), "second emission must be a no-op");
        assert!(matches!(
            *turn_state.lock().await,
            TurnState::Terminal(TerminalReason::Interrupted)
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn interrupt_called_twice_returns_same_response() {
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let handle = tokio::spawn(async { std::future::pending::<()>().await });
        let entry = test_active_turn(turn_id.clone(), handle.abort_handle());
        active_turns.lock().await.insert(session_id.clone(), entry);

        let first = decide_interrupt(
            &active_turns,
            &TurnInterruptParams {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            },
        )
        .await;
        assert!(matches!(first, InterruptOutcome::Captured { .. }));

        // Second call: state is Interrupting, so AlreadyInterrupting; no
        // double-emit, response shape is the idempotent `interrupted: true`.
        let second = decide_interrupt(
            &active_turns,
            &TurnInterruptParams {
                session_id,
                turn_id,
            },
        )
        .await;
        assert!(matches!(second, InterruptOutcome::AlreadyInterrupting));
        handle.abort();
    }

    #[tokio::test]
    async fn interrupt_mismatch_does_not_emit_invalid_params() {
        let session_id = SessionKey("local:test".into());
        let active_turn_id = TurnId::new();
        let other_turn_id = TurnId::new();
        let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let handle = tokio::spawn(async { std::future::pending::<()>().await });
        let entry = test_active_turn(active_turn_id.clone(), handle.abort_handle());
        active_turns.lock().await.insert(session_id.clone(), entry);

        let outcome = decide_interrupt(
            &active_turns,
            &TurnInterruptParams {
                session_id,
                turn_id: other_turn_id,
            },
        )
        .await;
        assert!(matches!(outcome, InterruptOutcome::Mismatch));
        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn interrupt_then_completion_race_emits_one_terminal() {
        // Drive 100 iterations of concurrent "natural-complete vs interrupt"
        // and assert: (a) exactly one terminal transition wins per iteration,
        // (b) at least one iteration actually exercises the race window —
        // i.e., the interrupt path captured first, then the completion path
        // observed `Interrupting` and converted it to `Terminal(Interrupted)`
        // (the original TOCTOU window between lookup and emission). A
        // `tokio::sync::Barrier` aligns the two tasks so they reliably
        // contend for the per-turn lock instead of running serially.
        let mut race_window_observed = 0;
        let mut completed_first = 0;
        let mut interrupted_first = 0;
        const ITERATIONS: usize = 100;
        for _ in 0..ITERATIONS {
            let turn_state = Arc::new(TokioMutex::new(TurnState::Active));
            let barrier = Arc::new(tokio::sync::Barrier::new(2));

            // Branch A: simulate the natural-completion path.
            let s_a = turn_state.clone();
            let b_a = barrier.clone();
            let task_a = tokio::spawn(async move {
                b_a.wait().await;
                transition_to_terminal(&s_a, TerminalReason::Completed).await
            });

            // Branch B: simulate the interrupt-handler path. First mutate to
            // `Interrupting` (decide_interrupt-style); then yield so the
            // turn-task path (branch A) has a chance to lock the state and
            // observe `Interrupting` before B's own transition emits. This
            // is precisely the original TOCTOU race window.
            let s_b = turn_state.clone();
            let b_b = barrier.clone();
            let task_b = tokio::spawn(async move {
                b_b.wait().await;
                let captured = {
                    let mut state = s_b.lock().await;
                    if matches!(*state, TurnState::Active) {
                        let (ack_tx, _ack_rx) = oneshot::channel();
                        *state = TurnState::Interrupting { ack: ack_tx };
                        true
                    } else {
                        false
                    }
                };
                if captured {
                    // Yield repeatedly — give the runtime an opportunity to
                    // schedule branch A on a different worker. Without this
                    // the same-task lock-release-acquire happens atomically
                    // from the runtime's POV and A never wins.
                    for _ in 0..4 {
                        tokio::task::yield_now().await;
                    }
                    transition_to_terminal(&s_b, TerminalReason::Interrupted).await
                } else {
                    None
                }
            });

            let (a, b) = tokio::try_join!(task_a, task_b).expect("tasks join");

            // Exactly one of the two transition calls must have actually
            // mutated state. Both being `Some` would be a double-emit bug.
            let mutations = [a.as_ref().is_some(), b.as_ref().is_some()]
                .iter()
                .filter(|&&x| x)
                .count();
            assert_eq!(mutations, 1, "exactly one terminal transition per turn");

            let terminal = match &*turn_state.lock().await {
                TurnState::Terminal(r) => *r,
                other => panic!("expected Terminal, got {other:?}"),
            };
            match terminal {
                TerminalReason::Completed => completed_first += 1,
                TerminalReason::Interrupted => interrupted_first += 1,
                TerminalReason::Errored => unreachable!(),
            }

            // Race window: branch A's transition reason is `Interrupted` —
            // it observed `Interrupting` set by branch B and converted it.
            // This is precisely the original TOCTOU window — under the old
            // code both `turn/completed` and `turn/error` would emit. Under
            // the new state machine, A reports `Interrupted` and B's second
            // transition is a no-op.
            if matches!(
                a.as_ref().map(|t| t.reason),
                Some(TerminalReason::Interrupted)
            ) {
                race_window_observed += 1;
            }
        }
        eprintln!(
            "interrupt-race repro: iterations={ITERATIONS} \
             race_window_observed={race_window_observed} \
             completed_first={completed_first} interrupted_first={interrupted_first}"
        );
        assert!(
            race_window_observed > 0,
            "expected at least one of {ITERATIONS} iterations to exercise the \
             race window (Completed-path observes Interrupting); got \
             completed_first={completed_first}, interrupted_first={interrupted_first}, \
             race_window={race_window_observed}"
        );
    }

    // ====================================================================
    // M9-FIX-06 — `approval_scope` enforcement (#644)
    //
    // These tests sit at the `(PendingApprovalStore, ScopePolicy)` integration
    // level. They mimic the exact recording sequence that
    // `handle_approval_respond` performs after a successful `respond`, then
    // probe `ScopePolicy::lookup` to verify auto-resolution. Going through
    // `handle_approval_respond` itself would require a real WebSocket sink;
    // the routing is exercised by the higher-level e2e suite.
    // ====================================================================

    /// Mirrors what `handle_approval_respond` does on success: respond to
    /// the pending approval and, if the scope is recordable, register the
    /// policy entry. Returns the recorded scope kind (or `None` if the
    /// scope was one-shot / unknown).
    fn respond_with_scope(
        contracts: &UiProtocolContractStores,
        params: ApprovalRespondParams,
    ) -> Option<ApprovalScopeKind> {
        let session_id = params.session_id.clone();
        let scope = params.approval_scope.clone();
        // FIX-01: `ApprovalDecision` is non-Copy (`Unknown(String)`); clone
        // out of `params` before `respond` consumes it.
        let decision = params.decision.clone();
        let outcome = contracts.approvals.respond(params).expect("respond ok");
        let scope = scope?;
        let context = outcome.context?;
        let kind = ApprovalScopeKind::from_scope_str(&scope);
        if !kind.is_recordable() {
            return None;
        }
        let key = match_key_for(kind, &context.tool_name, &context.turn_id);
        contracts.scopes.record(&session_id, kind, key, decision);
        Some(kind)
    }

    fn store_request(
        contracts: &UiProtocolContractStores,
        session_id: &SessionKey,
        approval_id: ApprovalId,
        turn_id: TurnId,
        tool: &str,
    ) {
        contracts.approvals.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id,
            turn_id,
            tool,
            "Run command",
            "cargo test",
        ));
    }

    #[test]
    fn scope_approve_for_turn_auto_resolves_within_turn() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_id.clone(),
            turn_id.clone(),
            "shell",
        );

        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
        params.approval_scope = Some("approve_for_turn".into());
        let kind = respond_with_scope(&contracts, params).expect("scope recorded");
        assert_eq!(kind, ApprovalScopeKind::ApproveForTurn);

        // Second approval in the same turn — same tool — should auto-resolve.
        let hit = contracts
            .scopes
            .lookup(&session_id, "shell", &turn_id)
            .expect("auto-resolve hit");
        assert_eq!(hit.decision, ApprovalDecision::Approve);
        assert_eq!(hit.scope_wire(), approval_scopes::TURN);
    }

    #[test]
    fn scope_approve_for_turn_re_prompts_on_next_turn() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_a = TurnId::new();
        let turn_b = TurnId::new();
        let approval_id = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_id.clone(),
            turn_a.clone(),
            "shell",
        );

        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
        params.approval_scope = Some("approve_for_turn".into());
        respond_with_scope(&contracts, params);

        // Same session but different turn → no auto-resolve; user must
        // re-affirm.
        assert!(
            contracts
                .scopes
                .lookup(&session_id, "shell", &turn_b)
                .is_none()
        );
    }

    #[test]
    fn scope_approve_for_session_persists_until_session_close() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_a = TurnId::new();
        let turn_b = TurnId::new();
        let approval_id = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_id.clone(),
            turn_a.clone(),
            "shell",
        );

        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
        params.approval_scope = Some("approve_for_session".into());
        respond_with_scope(&contracts, params);

        // Auto-resolve in turn A.
        assert!(
            contracts
                .scopes
                .lookup(&session_id, "shell", &turn_a)
                .is_some()
        );
        // Eviction-on-turn must NOT drop the session-scope entry.
        contracts.scopes.evict_turn(&session_id, &turn_a);
        assert!(
            contracts
                .scopes
                .lookup(&session_id, "shell", &turn_b)
                .is_some()
        );

        // Session close drops it.
        contracts.scopes.evict_session(&session_id);
        assert!(
            contracts
                .scopes
                .lookup(&session_id, "shell", &turn_b)
                .is_none()
        );
    }

    #[test]
    fn scope_approve_for_tool_auto_resolves_same_tool() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_a = TurnId::new();
        let turn_b = TurnId::new();
        let approval_id = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_id.clone(),
            turn_a.clone(),
            "shell",
        );

        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
        params.approval_scope = Some("approve_for_tool".into());
        respond_with_scope(&contracts, params);

        // Same tool, even on a different turn, auto-resolves.
        let hit = contracts
            .scopes
            .lookup(&session_id, "shell", &turn_b)
            .expect("tool scope persists across turns");
        assert_eq!(hit.scope_wire(), approval_scopes::TOOL);
        assert_eq!(hit.decision, ApprovalDecision::Approve);
    }

    #[test]
    fn scope_approve_for_tool_does_not_match_different_tool() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_id.clone(),
            turn_id.clone(),
            "shell",
        );

        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
        params.approval_scope = Some("approve_for_tool".into());
        respond_with_scope(&contracts, params);

        // Different tool name → no hit, must prompt again.
        assert!(
            contracts
                .scopes
                .lookup(&session_id, "browser", &turn_id)
                .is_none()
        );
    }

    #[test]
    fn scope_evicts_on_session_close() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_id.clone(),
            turn_id.clone(),
            "shell",
        );

        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
        params.approval_scope = Some("approve_for_session".into());
        respond_with_scope(&contracts, params);

        assert!(
            contracts
                .scopes
                .lookup(&session_id, "shell", &turn_id)
                .is_some()
        );
        contracts.scopes.evict_session(&session_id);
        assert!(
            contracts
                .scopes
                .lookup(&session_id, "shell", &turn_id)
                .is_none()
        );
    }

    #[test]
    fn unknown_scope_string_falls_back_to_approve_once() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_id.clone(),
            turn_id.clone(),
            "shell",
        );

        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
        // A scope token the server doesn't recognise — open-registry rule
        // says we MUST NOT error; we just don't record anything.
        params.approval_scope = Some("approve_for_galaxy_v9".into());
        let kind = respond_with_scope(&contracts, params);
        assert!(
            kind.is_none(),
            "unknown scope string must be treated as approve_once"
        );
        assert!(
            contracts
                .scopes
                .lookup(&session_id, "shell", &turn_id)
                .is_none()
        );
    }

    #[test]
    fn scope_approve_for_turn_evicted_when_finalize_turn_runs() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_id.clone(),
            turn_id.clone(),
            "shell",
        );
        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
        params.approval_scope = Some(approval_scopes::TURN.into());
        respond_with_scope(&contracts, params);

        contracts.scopes.evict_turn(&session_id, &turn_id);
        assert!(
            contracts
                .scopes
                .lookup(&session_id, "shell", &turn_id)
                .is_none(),
            "turn/completed must drop approve_for_turn entries"
        );
    }

    #[test]
    fn scope_deny_short_circuit_records_deny() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_id.clone(),
            turn_id.clone(),
            "shell",
        );
        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Deny);
        params.approval_scope = Some(approval_scopes::TOOL.into());
        respond_with_scope(&contracts, params);

        let hit = contracts
            .scopes
            .lookup(&session_id, "shell", &turn_id)
            .expect("deny scope hit");
        assert_eq!(hit.decision, ApprovalDecision::Deny);
    }

    #[test]
    fn scope_list_for_session_round_trips_via_handler_shape() {
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();

        let approval_a = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_a.clone(),
            turn_id.clone(),
            "shell",
        );
        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_a, ApprovalDecision::Approve);
        params.approval_scope = Some(approval_scopes::TURN.into());
        respond_with_scope(&contracts, params);

        let approval_b = ApprovalId::new();
        store_request(
            &contracts,
            &session_id,
            approval_b.clone(),
            turn_id.clone(),
            "shell",
        );
        let mut params =
            ApprovalRespondParams::new(session_id.clone(), approval_b, ApprovalDecision::Deny);
        params.approval_scope = Some(approval_scopes::TOOL.into());
        respond_with_scope(&contracts, params);

        let listed = contracts.scopes.list_for_session(&session_id);
        assert_eq!(listed.len(), 2);
        // Sorted by scope wire string ascending: tool < turn.
        assert_eq!(listed[0].scope, approval_scopes::TOOL);
        assert_eq!(listed[0].decision, ApprovalDecision::Deny);
        assert_eq!(listed[0].scope_match, "shell");
        assert_eq!(listed[1].scope, approval_scopes::TURN);
        assert_eq!(listed[1].decision, ApprovalDecision::Approve);
        assert_eq!(listed[1].turn_id.as_ref(), Some(&turn_id));
    }

    // ====================================================================
    // M9-FIX-04 — send-error handling + backpressure
    // ====================================================================

    /// Builds a `WsConnection` whose writer side feeds an in-test `mpsc`. The
    /// returned receiver is the "dedicated writer task" stand-in; drain it to
    /// unblock further sends, leave it alone to simulate a slow client.
    fn ws_connection_for_test(
        capacity: usize,
    ) -> (WsConnection, mpsc::Receiver<axum::extract::ws::Message>) {
        let (tx, rx) = mpsc::channel(capacity);
        (WsConnection::new(tx), rx)
    }

    #[tokio::test]
    async fn send_error_propagates_for_lifecycle_messages() {
        // capacity=1, the channel fills with the first frame; the second
        // lifecycle send must surface as `LifecycleFailure`. Without this
        // change, the bug was that callers `let _ =`'d the failure.
        let (ws, _rx) = ws_connection_for_test(1);

        // Fill the channel.
        let first = send_rpc_result(&ws, "1".into(), json!({"ok": true}));
        assert!(first.is_ok(), "first send must succeed");

        // Second lifecycle send should fail with LifecycleFailure (not be
        // silently dropped).
        let second = send_rpc_result(&ws, "2".into(), json!({"ok": true}));
        assert!(matches!(second, Err(SendError::LifecycleFailure(_))));
    }

    #[tokio::test]
    async fn send_error_logged_for_durable_notifications() {
        let (ws, _rx) = ws_connection_for_test(1);
        let ledger = UiProtocolLedger::new(16);
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();

        // Pre-fill capacity=1 channel.
        let first = send_notification_durable(
            &ws,
            &ledger,
            UiNotification::TurnStarted(octos_core::ui_protocol::TurnStartedEvent {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                timestamp: Utc::now(),
            }),
        );
        assert!(first.is_ok());

        // The second durable notification must be a BackpressureDrop and the
        // dropped count must increment so the next emit_replay_lossy* sees it.
        let second = send_notification_durable(
            &ws,
            &ledger,
            UiNotification::Warning(octos_core::ui_protocol::WarningEvent {
                session_id: session_id.clone(),
                turn_id: Some(turn_id.clone()),
                code: "test".into(),
                message: "drop me".into(),
            }),
        );
        assert!(matches!(second, Err(SendError::BackpressureDrop)));
        // The opportunistic replay_lossy attempt also fails (channel full), so
        // dropped_count is restored to >= 1 for a later flush.
        let metrics = ws.metrics();
        assert!(metrics.dropped_count.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn approval_request_backpressure_cancels_pending_runtime_waiter() {
        let (ws, _rx) = ws_connection_for_test(1);
        let ledger = UiProtocolLedger::new(16);
        let contracts = UiProtocolContractStores::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();

        let first = send_rpc_result(&ws, "fill".into(), json!({"ok": true}));
        assert!(first.is_ok(), "first send fills the bounded channel");

        let request = ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Run command",
            "cargo test",
        );
        let response_rx = contracts.approvals.request_runtime(request.clone());
        let send = send_notification_durable(
            &ws,
            &ledger,
            UiNotification::ApprovalRequested(request.clone()),
        );
        assert!(matches!(send, Err(SendError::BackpressureDrop)));

        cancel_approval_after_request_send_failure(
            &contracts,
            &ws,
            &ledger,
            &session_id,
            &approval_id,
            &turn_id,
        );

        assert!(
            response_rx.await.is_err(),
            "cancelling the pending approval drops the runtime sender"
        );
        assert!(
            contracts
                .approvals
                .pending_for_session(&session_id)
                .is_empty(),
            "failed sends must not leave a reconnect-pending approval"
        );
        let late_response = contracts
            .approvals
            .respond_with_context(ApprovalRespondParams::new(
                session_id.clone(),
                approval_id.clone(),
                ApprovalDecision::Approve,
            ))
            .expect_err("late response should see typed cancellation");
        assert_eq!(
            late_response.code,
            octos_core::ui_protocol::rpc_error_codes::APPROVAL_CANCELLED
        );
        assert_eq!(
            late_response.data.as_ref().unwrap()["reason"],
            APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED
        );

        let replay = ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 0,
                }),
            )
            .expect("replay after start cursor");
        assert!(replay.iter().any(|entry| matches!(
            &entry.event,
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(event))
                if event.approval_id == approval_id
        )));
        assert!(replay.iter().any(|entry| matches!(
            &entry.event,
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalCancelled(event))
                if event.approval_id == approval_id
                    && event.reason == APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED
        )));
    }

    #[tokio::test]
    async fn ephemeral_drops_are_silent_and_do_not_increment_dropped_count() {
        let (ws, _rx) = ws_connection_for_test(1);
        let ledger = UiProtocolLedger::new(16);
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();

        // Fill the channel with a non-ephemeral lifecycle frame.
        let first = send_rpc_result(&ws, "1".into(), json!({"ok": true}));
        assert!(first.is_ok());

        // Ephemeral message/delta drop: must surface as BackpressureDrop but
        // must NOT bump the dropped_count (ephemeral is non-durable per spec).
        let second = send_notification_ephemeral(
            &ws,
            &ledger,
            UiNotification::MessageDelta(MessageDeltaEvent {
                session_id,
                turn_id,
                text: "hi".into(),
            }),
        );
        assert!(matches!(second, Err(SendError::BackpressureDrop)));
        assert_eq!(ws.metrics().dropped_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn slow_client_does_not_wedge_other_connections() {
        // Two independent WsConnection wrappers (each with its own writer
        // channel + drainer) simulate two clients. Pause client A's drainer;
        // verify client B continues to receive frames during that window.
        let (ws_a, mut rx_a) = ws_connection_for_test(WS_WRITER_CHANNEL_CAPACITY);
        let (ws_b, mut rx_b) = ws_connection_for_test(WS_WRITER_CHANNEL_CAPACITY);
        let ledger = UiProtocolLedger::new(64);
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();

        // Spawn a "slow client A": sleeps 200ms before its first read. With
        // the old `Arc<Mutex<WsSink>>` pattern this would block all callers
        // because they held the lock across `.send().await`. With the new
        // mpsc design, each connection is independent.
        let slow_a = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let mut received = 0u32;
            while rx_a.try_recv().is_ok() {
                received += 1;
            }
            received
        });

        // While A is "paused", client B should continue to receive frames.
        for _ in 0..16u32 {
            let res = send_notification_durable(
                &ws_b,
                &ledger,
                UiNotification::Warning(octos_core::ui_protocol::WarningEvent {
                    session_id: session_id.clone(),
                    turn_id: Some(turn_id.clone()),
                    code: "tick".into(),
                    message: "for client B".into(),
                }),
            );
            assert!(res.is_ok(), "client B send must not be wedged by client A");
        }

        // Drain client B's channel to confirm frames did reach the writer side.
        let mut b_count = 0u32;
        while rx_b.try_recv().is_ok() {
            b_count += 1;
        }
        assert!(b_count >= 16, "client B received {b_count} frames");

        // Send something to A so the slow task has work. Sleep > 200ms total
        // by awaiting the join.
        let _ = send_notification_durable(
            &ws_a,
            &ledger,
            UiNotification::Warning(octos_core::ui_protocol::WarningEvent {
                session_id,
                turn_id: Some(turn_id),
                code: "tick".into(),
                message: "for client A".into(),
            }),
        );
        let a_received = slow_a.await.expect("slow client task");
        assert!(
            a_received >= 1,
            "client A eventually received {a_received} frames"
        );
    }

    #[tokio::test]
    async fn bounded_channel_full_emits_replay_lossy() {
        // Fill a small channel by never draining it; emit many durable
        // notifications. A `protocol/replay_lossy` frame must surface in the
        // channel before the test ends (opportunistic emit + flush).
        let (ws, mut rx) = ws_connection_for_test(8);
        let ledger = UiProtocolLedger::new(64);
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let progress_dropped = Arc::new(AtomicU64::new(0));

        // Pump 2000 durable notifications. Most will drop; the cumulative
        // count is held in `metrics.dropped_count`.
        for _ in 0..2000u32 {
            let _ = send_notification_durable(
                &ws,
                &ledger,
                UiNotification::Warning(octos_core::ui_protocol::WarningEvent {
                    session_id: session_id.clone(),
                    turn_id: Some(turn_id.clone()),
                    code: "tick".into(),
                    message: "load".into(),
                }),
            );
        }

        // Drain the channel — the replay_lossy frame may already be in there
        // from an opportunistic emit when capacity briefly opened.
        let mut frames = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            frames.push(msg);
        }

        // Now flush at the turn boundary (mimics what happens before
        // turn/completed). Any remaining drops must produce a replay_lossy.
        flush_replay_lossy(&ws, &ledger, &session_id, &progress_dropped);

        // After flush, drain again.
        while let Ok(msg) = rx.try_recv() {
            frames.push(msg);
        }

        // At least one frame in the captured set must be a `protocol/replay_lossy`.
        let lossy_frame = frames.iter().find_map(|frame| match frame {
            axum::extract::ws::Message::Text(text)
                if text.as_str().contains("\"protocol/replay_lossy\"") =>
            {
                Some(text.as_str().to_string())
            }
            _ => None,
        });
        assert!(
            lossy_frame.is_some(),
            "expected a protocol/replay_lossy frame among {} captured",
            frames.len()
        );
        // Surface a sample for the M9 status report — useful when running
        // with `-- --nocapture`.
        if let Some(sample) = lossy_frame {
            eprintln!("sample protocol/replay_lossy frame: {sample}");
        }
    }

    #[test]
    fn replay_lossy_method_is_registered_in_core_protocol() {
        // Schema-side guard: the new method name and notification variant
        // must be wired into the core protocol's notification list and
        // dispatch table. Catches "added the variant but forgot the entry"
        // regressions.
        let methods = octos_core::ui_protocol::UI_PROTOCOL_NOTIFICATION_METHODS;
        assert!(methods.contains(&octos_core::ui_protocol::methods::REPLAY_LOSSY));

        let event = UiNotification::ReplayLossy(ReplayLossyEvent {
            session_id: SessionKey("local:test".into()),
            dropped_count: 7,
            last_durable_cursor: Some(UiCursor {
                stream: "local:test".into(),
                seq: 42,
            }),
        });
        let frame = event
            .into_rpc_notification()
            .expect("serialize replay_lossy");
        assert_eq!(frame.method, octos_core::ui_protocol::methods::REPLAY_LOSSY);
        assert_eq!(frame.params["dropped_count"], json!(7));
        assert_eq!(frame.params["last_durable_cursor"]["seq"], json!(42));
    }

    // ====================================================================
    // M9-FIX-07 — approval decision audit log + replay
    // ====================================================================

    #[test]
    fn audit_log_records_every_decision() {
        // Mirrors what `handle_approval_respond` does. Verifies one
        // JSON-Lines entry per decision and that no payload bodies leak.
        use octos_core::ui_protocol::ApprovalRequestedEvent;

        let temp = tempfile::tempdir().expect("tempdir");
        let log = ApprovalsAuditLog::new(temp.path(), ApprovalsAuditConfig::default());
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:audit".into());

        let mut ids = Vec::new();
        for _ in 0..3 {
            let approval_id = ApprovalId::new();
            ids.push(approval_id.clone());
            approvals.request(ApprovalRequestedEvent::generic(
                session_id.clone(),
                approval_id.clone(),
                TurnId::new(),
                "shell",
                "Run",
                "secret-body",
            ));
            let params = ApprovalRespondParams::new(
                session_id.clone(),
                approval_id,
                ApprovalDecision::Approve,
            );
            let outcome = approvals
                .respond_with_context(params.clone())
                .expect("decide");
            let event = crate::api::ui_protocol_approvals::build_decided_event(
                &params,
                &outcome,
                "user:test",
                chrono::Utc::now(),
            );
            let tool_name = outcome.context.as_ref().map(|ctx| ctx.tool_name.clone());
            log.record(&event, tool_name.as_deref()).expect("write");
        }

        let active = std::fs::read_dir(temp.path().join("audit"))
            .expect("audit dir")
            .filter_map(Result::ok)
            .next()
            .expect("active log")
            .path();
        let lines = crate::api::ui_protocol_audit::read_audit_lines(&active);
        assert_eq!(lines.len(), 3);
        for (line, expected_id) in lines.iter().zip(ids.iter()) {
            assert_eq!(line["approval_id"], json!(expected_id.0.to_string()));
            assert_eq!(line["decision"], json!("approve"));
            assert_eq!(line["tool_name"], json!("shell"));
            assert_eq!(line["auto_resolved"], json!(false));
            // PII rule: no command body fields, no body content.
            assert!(!serde_json::to_string(line).unwrap().contains("secret-body"));
        }
    }

    #[tokio::test]
    async fn reconnect_after_decision_replays_decided_event() {
        use chrono::Utc;
        use octos_core::ui_protocol::{ApprovalDecidedEvent, ApprovalRequestedEvent};

        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(64);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:reconnect".into());
        let approval_id = ApprovalId::new();
        let turn_id = TurnId::new();

        // Seed a pre-C1 anchor so the reconnect cursor can express "before
        // C1" — the cursor space starts at 1.
        let warmup = ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            text: "preamble".into(),
        }));
        let request = ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id,
            "shell",
            "Run command",
            "cargo test",
        );
        approvals.request(request.clone());
        ledger.append_notification(UiNotification::ApprovalRequested(request));
        let outcome_decide = approvals
            .respond_with_context(ApprovalRespondParams::new(
                session_id.clone(),
                approval_id.clone(),
                ApprovalDecision::Approve,
            ))
            .expect("decide");
        let decided_turn_id = outcome_decide
            .context
            .as_ref()
            .map(|ctx| ctx.turn_id.clone())
            .expect("request was registered");
        ledger.append_notification(UiNotification::ApprovalDecided(ApprovalDecidedEvent {
            session_id: session_id.clone(),
            approval_id: approval_id.clone(),
            turn_id: decided_turn_id,
            decision: ApprovalDecision::Approve,
            scope: Some("session".into()),
            decided_at: Utc::now(),
            decided_by: "user:tester".into(),
            auto_resolved: false,
            policy_id: None,
            client_note: None,
        }));

        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: None,
                cwd: None,
                after: Some(warmup.cursor.clone()),
            },
        )
        .await
        .expect("reconnect should succeed");

        let mut saw_requested = false;
        let mut saw_decided = false;
        for event in &outcome.replay {
            match &event.event {
                UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(e))
                    if e.approval_id == approval_id =>
                {
                    saw_requested = true;
                }
                UiProtocolLedgerEvent::Notification(UiNotification::ApprovalDecided(e))
                    if e.approval_id == approval_id =>
                {
                    saw_decided = true;
                    assert_eq!(e.decision, ApprovalDecision::Approve);
                    assert_eq!(e.scope.as_deref(), Some("session"));
                }
                _ => {}
            }
        }
        assert!(saw_requested, "replay missing approval/requested");
        assert!(saw_decided, "replay missing approval/decided");
        assert!(outcome.pending_approvals.is_empty());
    }

    // ====================================================================
    // M9-06 — terminal task lifecycle durability under WS backpressure
    // ====================================================================

    fn make_background_task(
        id: &str,
        status: octos_agent::TaskStatus,
        runtime_state: octos_agent::TaskRuntimeState,
    ) -> octos_agent::BackgroundTask {
        octos_agent::BackgroundTask {
            id: id.into(),
            tool_name: "deep_search".into(),
            tool_call_id: "call-1".into(),
            parent_session_key: Some("local:test".into()),
            child_session_key: None,
            child_terminal_state: None,
            child_join_state: None,
            child_joined_at: None,
            child_failure_action: None,
            task_ledger_path: None,
            status,
            runtime_state,
            runtime_detail: None,
            started_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            completed_at: None,
            output_files: Vec::new(),
            error: None,
            session_key: Some("local:test".into()),
            tool_input: None,
        }
    }

    /// FIX-06: when the progress channel is full and a *terminal* task
    /// snapshot arrives, the helper must keep the update durable — `try_send`
    /// fails fast, then a spawned awaited send delivers it once the consumer
    /// drains a slot. Pre-fix, the bare `try_send` dropped the terminal
    /// update and the UI was stuck on `running` forever.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn terminal_task_update_survives_backpressure() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);
        let dropped = Arc::new(AtomicU64::new(0));

        // Fill the channel so the next try_send fails.
        tx.try_send("filler".into()).expect("fill channel");

        let task = make_background_task(
            "01900000-0000-7000-8000-0000000000aa",
            octos_agent::TaskStatus::Completed,
            octos_agent::TaskRuntimeState::Completed,
        );
        forward_task_progress_to_channel(&tx, &dropped, &task);

        // The synchronous try_send must have failed (channel was full),
        // bumping the drop counter that feeds the replay_lossy machinery.
        assert_eq!(
            dropped.load(Ordering::Relaxed),
            1,
            "immediate try_send failure must increment the drop counter so replay_lossy stays accurate"
        );

        // Drain the filler to make room for the spawned awaited send.
        let filler = rx.recv().await.expect("filler must be there");
        assert_eq!(filler, "filler");

        // Yield the runtime so the spawned send task gets to run, then
        // advance virtual time within the timeout budget.
        tokio::time::advance(std::time::Duration::from_millis(50)).await;

        // The terminal update must arrive.
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("terminal update must be delivered within timeout")
            .expect("channel must still be open");
        let parsed: serde_json::Value = serde_json::from_str(&terminal).expect("valid json");
        assert_eq!(parsed["type"], "task_updated");
        assert_eq!(parsed["task_id"], "01900000-0000-7000-8000-0000000000aa");
        assert_eq!(parsed["state"], "ready"); // Completed -> Ready in the lifecycle mapping
    }

    /// Pin the existing behavior for *non-terminal* updates: under
    /// backpressure they MAY be dropped (the next update will overwrite),
    /// and the drop must be visible via the counter + metric so the WS
    /// layer can flush a `protocol/replay_lossy` later.
    #[tokio::test(flavor = "current_thread")]
    async fn non_terminal_update_drops_under_backpressure_and_increments_counter() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);
        let dropped = Arc::new(AtomicU64::new(0));

        // Fill the channel.
        tx.try_send("filler".into()).expect("fill channel");

        let task = make_background_task(
            "01900000-0000-7000-8000-0000000000bb",
            octos_agent::TaskStatus::Running,
            octos_agent::TaskRuntimeState::ExecutingTool,
        );
        forward_task_progress_to_channel(&tx, &dropped, &task);

        // Drop counter must increment — same as before the fix.
        assert_eq!(dropped.load(Ordering::Relaxed), 1);

        // Now drain the filler. There must be NO pending non-terminal send
        // queued behind it; the helper's contract is "drop is fine for
        // non-terminal" and we don't want a spawned-await on every running
        // update piling up zombie tasks.
        let filler = rx.recv().await.expect("filler must be present");
        assert_eq!(filler, "filler");

        // Give any (incorrectly) spawned send task a chance to run, then
        // assert nothing follows.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let next = rx.try_recv();
        assert!(
            next.is_err(),
            "non-terminal updates must not be durably retried under backpressure (got {next:?})"
        );
    }

    /// Sanity-check the fast path: when the channel has capacity, the
    /// helper sends synchronously without spawning anything and without
    /// touching the drop counter.
    #[tokio::test(flavor = "current_thread")]
    async fn task_update_fast_path_when_channel_has_capacity() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
        let dropped = Arc::new(AtomicU64::new(0));

        let task = make_background_task(
            "01900000-0000-7000-8000-0000000000cc",
            octos_agent::TaskStatus::Failed,
            octos_agent::TaskRuntimeState::Failed,
        );
        forward_task_progress_to_channel(&tx, &dropped, &task);

        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        let event = rx.try_recv().expect("event must be available immediately");
        let parsed: serde_json::Value = serde_json::from_str(&event).expect("valid json");
        assert_eq!(parsed["state"], "failed");
    }
}
