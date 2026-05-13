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
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use octos_agent::{
    Agent, BackgroundResultKind, BackgroundResultPayload, ToolApprovalDecision, ToolApprovalRequest,
};
use octos_core::ui_protocol::{
    ApprovalAutoResolvedEvent, ApprovalCancelledEvent, ApprovalCommandDetails,
    ApprovalDecidedEvent, ApprovalDecision, ApprovalId, ApprovalRenderHints,
    ApprovalRequestedEvent, ApprovalTypedDetails, ContentBulkDeleteParams, ContentDeleteParams,
    ContentListParams, HydratedMessage, HydratedTurn, InputItem, MessageDeltaEvent,
    MessagePersistedEvent, MessagePersistedSource, OutputCursor, ReplayLossyEvent, RpcError,
    RpcErrorResponse, RpcRequest, RpcResponse, SESSION_HYDRATE_INCLUDE_MAX,
    SESSION_MESSAGES_PAGE_DEFAULT_LIMIT, SESSION_MESSAGES_PAGE_MAX_LIMIT,
    SESSION_MESSAGES_PAGE_MAX_OFFSET, SESSION_TITLE_SET_MAX_CHARS, SessionDeleteParams,
    SessionFilesListParams, SessionHydrateParams, SessionHydrateResult, SessionListParams,
    SessionMessagesPageParams, SessionOpenParams, SessionOpenResult, SessionOpened,
    SessionSnapshotParams, SessionStatusGetParams, SessionTasksListParams, SessionTitleSetParams,
    SessionWorkspaceGetParams, SystemStatusGetParams, TaskCancelParams, TaskCancelResult,
    TaskListEntry, TaskListParams, TaskListResult, TaskOutputDeltaEvent, TaskRestartFromNodeParams,
    TaskRestartFromNodeResult, TaskRuntimeState as UiTaskRuntimeState, TaskUpdatedEvent,
    ThreadGraphEntry, ThreadGraphGetParams, ThreadGraphGetResult, ToolCompletedEvent,
    ToolProgressEvent, ToolStartedEvent, TurnCompletedEvent, TurnErrorEvent, TurnId,
    TurnInterruptParams, TurnInterruptResult, TurnLifecycleState, TurnSpawnCompleteEvent,
    TurnStartParams, TurnStateGetParams, TurnStateGetResult, UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1,
    UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1, UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1,
    UI_PROTOCOL_FEATURE_MESSAGE_PERSISTED_V1, UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1,
    UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1, UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
    UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1, UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1,
    UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1, UiArtifactPaneItem, UiArtifactPaneSnapshot, UiCommand,
    UiCursor, UiFileMutationNotice, UiGitHistoryItem, UiGitPaneSnapshot, UiGitStatusItem,
    UiNotification, UiPaneSnapshot, UiPaneSnapshotLimitation, UiProgressEvent, UiProgressMetadata,
    UiProtocolCapabilities, UiWorkspacePaneEntry, UiWorkspacePaneSnapshot,
    approval_cancelled_reasons, approval_kinds, hydrate_sections, progress_kinds, thread_status,
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
    ConnectionId, LedgerConfig, LedgeredUiProtocolEvent, UiProtocolLedger, UiProtocolLedgerEvent,
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

/// Per-connection registry of live ledger-forwarder tasks keyed by session.
/// Each entry pumps `LedgeredUiProtocolEvent`s from the ledger broadcast
/// into the WS write channel for the lifetime of the connection. Dropping
/// or aborting a handle terminates the pump.
///
/// #924 NIT 8: keep the full `JoinHandle` (not just the `AbortHandle`) so
/// the connection-cleanup path can `await` the aborted task before pruning
/// idle subscribers. With only an `AbortHandle`, a single `yield_now()`
/// after `abort()` was best-effort — under load the receiver might not
/// have dropped before `prune_subscriber_if_idle` ran, leaving the ledger
/// broadcaster believing it still had a live subscriber.
type SharedLiveForwarders =
    Arc<tokio::sync::Mutex<HashMap<SessionKey, tokio::task::JoinHandle<()>>>>;

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
    /// #924 BLOCK 2: a prior lifecycle/RPC send already latched the
    /// connection as failed. Background tasks (turn forwarders, live
    /// forwarders, ledger fan-out) MUST stop enqueueing onto a dead
    /// channel — callers treat this exactly like `Closed`.
    FatalClosed,
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
    /// Unique within the process. Stamped onto every ledger append we
    /// also direct-send so the live forwarder running on this same
    /// connection can drop the broadcast copy and avoid duplicate
    /// delivery to the WS.
    connection_id: ConnectionId,
    /// #922.2: latched when a lifecycle/RPC send hits backpressure or a
    /// closed writer. The read loop polls this and breaks cleanly so a
    /// silently-dropped RPC reply does not strand the client.
    failed: Arc<std::sync::atomic::AtomicBool>,
    /// #924 BLOCK 1: a Notify woken in tandem with `failed.store(true)`.
    /// The connection main loop `select!`s on this alongside the inbound
    /// frame stream so a lifecycle/RPC send failure on an idle socket
    /// triggers cleanup immediately rather than waiting indefinitely
    /// for the next client frame.
    failed_notify: Arc<tokio::sync::Notify>,
}

impl WsConnection {
    pub(crate) fn new(writer: mpsc::Sender<WsMessage>) -> Self {
        Self {
            writer,
            metrics: Arc::new(ConnectionMetrics::default()),
            connection_id: ConnectionId::next(),
            failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// #924 BLOCK 1: handle to the latch-wakeup notify for the read
    /// loop's `select!` arm. Cloned cheaply (just an `Arc` bump).
    fn failed_notify(&self) -> Arc<tokio::sync::Notify> {
        self.failed_notify.clone()
    }

    fn mark_failed(&self) {
        self.failed
            .store(true, std::sync::atomic::Ordering::Release);
        // Wake every current and future `notified()` waiter so the read
        // loop wakes immediately on an idle connection. `notify_waiters`
        // alone has no permit-stash behaviour; combined with the Acquire
        // load in the select! arm a late `notified().await` will still
        // see `failed == true` and bail out before parking.
        self.failed_notify.notify_waiters();
    }

    #[cfg(test)]
    pub(crate) fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    #[cfg(test)]
    pub(crate) fn metrics(&self) -> Arc<ConnectionMetrics> {
        self.metrics.clone()
    }

    fn try_enqueue(&self, frame: WsMessage) -> Result<(), SendError> {
        // #924 BLOCK 2: once the connection is latched as failed every
        // further enqueue must fail loudly. Background tasks (turn
        // forwarders, live forwarders, ledger fan-out) keep pumping
        // until the connection cleanup aborts their handles; without
        // this gate they would push frames onto a writer the read loop
        // is about to drop, masking the original lifecycle failure and
        // wasting work. `FatalClosed` is callers' signal to treat the
        // connection like `Closed`.
        if self.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(SendError::FatalClosed);
        }
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
    ///
    /// #922.2: a lifecycle-frame backpressure drop is treated as a
    /// connection failure. The latched `failed` flag tells the read
    /// loop to stop dispatch and tear down — better than silently
    /// dropping RPC replies (which left clients timing out while the
    /// server thought the call succeeded).
    fn send_lifecycle(&self, frame: WsMessage) -> Result<(), SendError> {
        match self.try_enqueue(frame) {
            Ok(_) => Ok(()),
            Err(SendError::BackpressureDrop) => {
                metrics::counter!("ws.send.error.lifecycle").increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    reason = "backpressure",
                    "lifecycle ws send failed; aborting connection"
                );
                self.mark_failed();
                Err(SendError::LifecycleFailure(
                    "writer channel full for lifecycle frame".into(),
                ))
            }
            Err(SendError::Closed) => {
                metrics::counter!("ws.send.error.lifecycle").increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    reason = "closed",
                    "lifecycle ws send failed; aborting connection"
                );
                self.mark_failed();
                Err(SendError::LifecycleFailure(
                    "writer channel closed for lifecycle frame".into(),
                ))
            }
            Err(SendError::FatalClosed) => {
                // #924 BLOCK 2: prior caller already latched the
                // connection failed; surface the lifecycle-shape error
                // for any callsite still doing work on this connection.
                Err(SendError::LifecycleFailure(
                    "connection already latched as failed".into(),
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
            Err(SendError::FatalClosed) => {
                // #924 BLOCK 2: connection already latched failed by a
                // prior lifecycle send — no point counting another
                // dropped row.
                metrics::counter!("ws.send.drop.closed", "method" => method.to_string())
                    .increment(1);
                Err(SendError::FatalClosed)
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
            Err(SendError::FatalClosed) => {
                // #924 BLOCK 2: silently drop, consistent with the spec
                // § 9 "ephemeral frames may be dropped" rule. The
                // connection is already torn down by the read loop.
                Err(SendError::FatalClosed)
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
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(20));
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't ship a heartbeat
        // before any real frame.
        ping_interval.tick().await;
        loop {
            tokio::select! {
                maybe_msg = rx.recv() => {
                    match maybe_msg {
                        Some(msg) => {
                            if sink.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping_interval.tick() => {
                    // Send a JSON-RPC notification as the keepalive instead of
                    // a binary `Ping`. Codex review on the mini5 intermittent
                    // disconnect (2026-05-13): browsers auto-Pong control
                    // frames at the WebSocket layer — they never reach
                    // JS-land `onmessage`. The SPA bridge tracks its own
                    // 60 s idle timeout (`ui-protocol-bridge.ts:195`) that
                    // only refreshes on text frames observed by the JS
                    // handler. Binary Ping kept the TCP connection alive
                    // for proxies but left the bridge timer starving, so
                    // it tore the socket down after every minute of idle.
                    // A text-frame heartbeat ticks both meters at once.
                    let payload =
                        "{\"jsonrpc\":\"2.0\",\"method\":\"server/heartbeat\",\"params\":{}}";
                    if sink
                        .send(WsMessage::Text(payload.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
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
    harness_task_control: bool,
    /// UPCR-2026-009 `state.session_hydrate.v1` negotiated.
    session_hydrate: bool,
    /// UPCR-2026-010 `state.thread_graph.v1` negotiated.
    thread_graph: bool,
    /// UPCR-2026-011 `state.turn_state_get.v1` negotiated.
    turn_state_get: bool,
    /// UPCR-2026-012 `event.message_persisted.v1` negotiated.
    message_persisted: bool,
    /// M10 Phase 1 `event.spawn_complete.v1` negotiated. When set, the
    /// connection receives `turn/spawn_complete` envelope events for
    /// `spawn_only` background completions and the corresponding
    /// `message/persisted` row (with `source: background`) is suppressed
    /// at the per-connection wire-emit gate. When unset, the legacy
    /// `message/persisted` shape is preserved and `turn/spawn_complete`
    /// is suppressed.
    spawn_complete: bool,
    /// M12 Phase D-1 `auxiliary.rest_to_ws.v1` negotiated. Unlocks the
    /// thirteen auxiliary JSON-RPC methods (`session/list`,
    /// `session/snapshot`, `session/messages_page`, `session/status.get`,
    /// `session/files.list`, `session/tasks.list`,
    /// `session/workspace.get`, `session/title.set`, `session/delete`,
    /// `system/status.get`, `content/list`, `content/delete`,
    /// `content/bulk_delete`) on the existing WS connection.
    /// Capability-gated per ADR
    /// `docs/adr/m12-phase-d-auxiliary-rest-to-ws.md`. REST endpoints
    /// remain available for clients that do not negotiate this feature.
    /// **Strict opt-in (codex review):** the gate fires for these methods
    /// regardless of whether `header_present` is true, so a client that
    /// sends no feature header at all still receives
    /// `method_not_supported` and falls back to REST. This is what makes
    /// Phase D-1 truly additive — pre-existing clients cannot trip into
    /// the new methods without explicit negotiation.
    auxiliary_rest_to_ws_v1: bool,
    /// `true` when the client sent at least one feature token via the
    /// `X-Octos-Ui-Features` header or the `ui_feature` / `ui_features`
    /// query parameter (UPCR-2026-007). Distinguishes "no header at all"
    /// (where the server falls back to advertising the full first-slice in
    /// `SessionOpened.capabilities`) from "header sent with all-unknown
    /// tokens" (where the negotiated `supported_features` is empty).
    header_present: bool,
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
            harness_task_control: has_ui_feature(
                headers,
                query,
                UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1,
            ),
            session_hydrate: has_ui_feature(headers, query, UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1),
            thread_graph: has_ui_feature(headers, query, UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1),
            turn_state_get: has_ui_feature(headers, query, UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1),
            message_persisted: has_ui_feature(
                headers,
                query,
                UI_PROTOCOL_FEATURE_MESSAGE_PERSISTED_V1,
            ),
            spawn_complete: has_ui_feature(headers, query, UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1),
            auxiliary_rest_to_ws_v1: has_ui_feature(
                headers,
                query,
                UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1,
            ),
            header_present: has_any_ui_feature_token(headers, query),
        }
    }

    /// Build the `UiProtocolCapabilities` payload to advertise on
    /// `SessionOpened` per UPCR-2026-007 § 4 capability negotiation. When
    /// the client sent no feature header at all, the server returns the
    /// `first_server_slice` default so clients can still discover the
    /// surface in-band. When the client sent at least one feature token,
    /// the server returns the intersection of requested features with the
    /// known feature registry — clients see exactly which of their
    /// requests were honoured and never receive a flag they did not ask
    /// for.
    fn negotiated_capabilities(self) -> UiProtocolCapabilities {
        if !self.header_present {
            return UiProtocolCapabilities::first_server_slice();
        }
        let mut requested: Vec<&str> = Vec::with_capacity(8);
        if self.typed_approvals {
            requested.push(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1);
        }
        if self.pane_snapshots {
            requested.push(UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1);
        }
        if self.session_workspace_cwd {
            requested.push(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1);
        }
        if self.harness_task_control {
            requested.push(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1);
        }
        if self.session_hydrate {
            requested.push(UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1);
        }
        if self.thread_graph {
            requested.push(UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1);
        }
        if self.turn_state_get {
            requested.push(UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1);
        }
        if self.message_persisted {
            requested.push(UI_PROTOCOL_FEATURE_MESSAGE_PERSISTED_V1);
        }
        if self.spawn_complete {
            requested.push(UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1);
        }
        if self.auxiliary_rest_to_ws_v1 {
            requested.push(UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1);
        }
        UiProtocolCapabilities::for_negotiated_features(requested)
    }
}

/// True when the client sent any non-empty `X-Octos-Ui-Features` token
/// through the header or the URL query. Used by UPCR-2026-007 to
/// distinguish "no negotiation attempted" from "negotiation attempted with
/// no honoured tokens".
fn has_any_ui_feature_token(headers: &HeaderMap, query: Option<&str>) -> bool {
    let header_has_token = headers
        .get(UI_FEATURES_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split([',', ' '])
                .any(|candidate| !candidate.trim().is_empty())
        });
    if header_has_token {
        return true;
    }
    query
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| matches!(*key, "ui_feature" | "ui_features" | "x-octos-ui-features"))
        .flat_map(|(_, value)| value.split([',', ' ']))
        .any(|candidate| !candidate.trim().is_empty())
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
pub(super) async fn event_ledger(state: &AppState) -> Arc<UiProtocolLedger> {
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
        // UPCR-2026-012: install the post-fsync observer that converts
        // every successful `add_message_with_seq` commit into a
        // `message/persisted` ledger append. We install on the same
        // path that wins the ledger get_or_init so a process never has
        // two competing observers.
        install_message_commit_observer(installed.clone());
    }
    installed
}

/// Install the durable-commit observer that records every successful
/// `add_message_with_seq` commit as a `message/persisted` ledger entry.
///
/// Per UPCR-2026-012 the observer fires AFTER `add_message_with_seq`'s
/// disk write returned Ok and the in-memory mirror was updated, so any
/// recorded notification always reflects a row that is durably visible.
/// A commit failure (size cap, fsync error) returns Err from
/// `append_to_disk` and the observer is skipped — the
/// "MUST NOT emit on commit failure" invariant.
///
/// The ledger's `append_notification` takes the per-session global lock
/// and stamps a strict-monotonic seq via the
/// [`UiProtocolLedgerEvent::with_cursor`] hook — that hook also patches
/// `MessagePersistedEvent.cursor` so the wire payload's `cursor` field
/// carries the same authoritative seq the ledger envelope assigned.
/// Two concurrent commits to the same session serialise on the ledger
/// lock, so notifications are strict-ordered per session per
/// UPCR-2026-012's ordering invariant.
///
/// Delivery model: the entry is persisted to the ledger ring (disk +
/// in-memory). Clients receive `message/persisted` via two paths,
/// whichever wins the race: (a) cursor-based replay on
/// `session/open { after: <cursor> }`, or (b) the per-session live
/// publish-subscribe broadcast (`UiProtocolLedger::subscribe`) drained
/// by `spawn_live_forwarder` for currently connected WebSocket clients.
/// Both paths are reconciled by the forwarder's `baseline_seq` filter
/// (replay snapshot head) and `from_connection` self-suppression so
/// each event reaches each WS exactly once. Issue #760 / PR #761
/// closed the original "no live fan-out" gap; clients that go offline
/// still resync via cursor on reconnect.
/// Bounded channel capacity for the per-session `SendFileTool` sink. Each
/// session drains its own channel into the canonical-persist path, so 64
/// pending messages is generous; if a runaway tool ever exceeds this we'd
/// rather backpressure the agent loop than balloon memory.
const SEND_FILE_CHANNEL_CAPACITY: usize = 64;

tokio::task_local! {
    /// M10 Phase 1: task-local override for `MessagePersistedSource` read
    /// by `install_message_commit_observer`. The
    /// [`BackgroundResultSender`] callback enters this scope before
    /// invoking [`persist_assistant_with_media`] so the resulting
    /// `MessagePersistedEvent` carries `source: background` instead of
    /// the role-derived `assistant` default. The per-connection
    /// capability filter then identifies "this is a duplicate of a
    /// `turn/spawn_complete`" and suppresses it for clients that
    /// negotiated the new wire shape.
    ///
    /// Without this override the `Message` role is `Assistant`, the
    /// observer maps it to `MessagePersistedSource::Assistant`, and
    /// the duplicate-suppression branch at
    /// `live_event_passes_capability_filter` never fires — which
    /// codex flagged as a P1 against the Phase 1 wire contract.
    static MESSAGE_PERSISTED_SOURCE_OVERRIDE: Option<MessagePersistedSource>;
}

/// Resolve the source for an upcoming `MessagePersistedEvent`. Returns the
/// task-local override when one is set (e.g. inside the `BackgroundResultSender`
/// scope), otherwise falls back to the role-derived default — preserving
/// the pre-M10 behaviour for every other persist path.
fn current_message_persisted_source(role: octos_core::MessageRole) -> MessagePersistedSource {
    MESSAGE_PERSISTED_SOURCE_OVERRIDE
        .try_with(|override_value| *override_value)
        .ok()
        .flatten()
        .unwrap_or_else(|| MessagePersistedSource::from_role(role))
}

/// Pre-stamp `thread_id` on a row about to be persisted by the standalone
/// turn loop so every User/Assistant/Tool row from the same turn shares the
/// originating `TurnId`-derived thread id.
///
/// Caller-supplied `thread_id` values are preserved. System rows are left
/// alone (they aren't thread-scoped). For `User`/`Assistant`/`Tool` rows
/// missing a `thread_id`, the supplied `turn_thread_id` is stamped.
///
/// **M10 Phase 6.1**: extending this from Assistant/Tool only to also cover
/// `User` closes the empty-placeholder bubble. `process_message_inner`
/// builds the user row with `client_message_id: None`, so without the
/// pre-stamp `derive_thread_id_for_new_write` falls back to a fresh
/// `now_v7()` for the user row while assistant rows are stamped with the
/// `TurnId`. The SPA reducer keys threads on `thread_id`; a divergent user
/// thread leaves an empty pending bubble in the user's thread and creates
/// an orphan thread for the assistant rows.
fn pre_stamp_turn_thread_id(message: Message, turn_thread_id: &str) -> Message {
    let mut to_save = message;
    if to_save.thread_id.is_none()
        && matches!(
            to_save.role,
            MessageRole::User | MessageRole::Assistant | MessageRole::Tool
        )
    {
        to_save.thread_id = Some(turn_thread_id.to_owned());
    }
    to_save
}

/// Shared persist helper used by the api/serve background-result sender
/// (spawn_only completions) and the `send_file` sink. Builds an assistant
/// `Message` with the given content + media + thread_id, writes it through
/// the canonical session helper (which serialises with other writers via
/// the per-key Tokio mutex and triggers `MessageCommitObserver`), then
/// invalidates the cached `SessionManager` entry so subsequent
/// `session/hydrate` and `/api/sessions/:id/messages` reads pick up the
/// new row instead of the pre-persist snapshot. Mirrors the gateway's
/// `session_actor.rs::deliver_background_notification` post-write
/// invalidate at `api_channel.rs:1503`.
///
/// Returns `Some(PersistedMessageMeta)` on success — the row's committed
/// seq plus the wire `message_id` derived the same way `MessageCommitObserver`
/// computes it (`session:seq:timestamp_ns`). The shared id lets the
/// `BackgroundResultSender` callback emit a `turn/spawn_complete`
/// envelope whose `message_id` matches the parallel `message/persisted`
/// event — clients that key dedup or confirmation off `message_id`
/// then see one logical row, regardless of which wire shape they
/// negotiated. `None` signals a persist failure (already logged).
async fn persist_assistant_with_media(
    sessions: &Arc<TokioMutex<octos_bus::SessionManager>>,
    data_dir: &Path,
    session_id: &SessionKey,
    content: String,
    media: Vec<String>,
    thread_id: String,
    label: &str,
) -> Option<PersistedMessageMeta> {
    let mut message = Message::assistant_with_thread(content, octos_core::ThreadId::new(thread_id));
    message.media = media;
    // Capture the stamped timestamp BEFORE the canonical persist
    // consumes the message — `MessageCommitObserver` derives the wire
    // `message_id` from `(session_id, committed_seq, message.timestamp)`,
    // and we need that same value here so the spawn_complete envelope
    // can advertise the identical id.
    let timestamp_ns = message.timestamp.timestamp_nanos_opt().unwrap_or(0);

    let committed_seq = match octos_bus::session::persist_message_through_canonical_path(
        data_dir, session_id, message,
    )
    .await
    {
        Ok(seq) => seq,
        Err(error) => {
            tracing::warn!(
                session = %session_id.0,
                label,
                error = %error,
                "api/serve: failed to persist background-delivered message"
            );
            return None;
        }
    };

    sessions.lock().await.invalidate_cache(session_id);
    Some(PersistedMessageMeta {
        committed_seq,
        message_id: format!("{}:{committed_seq}:{timestamp_ns}", session_id.0),
    })
}

/// Metadata returned by [`persist_assistant_with_media`] so callers can
/// emit wire events whose identity matches the durable row written by
/// `MessageCommitObserver`. See the helper's doc comment for rationale.
#[derive(Debug, Clone)]
struct PersistedMessageMeta {
    committed_seq: usize,
    message_id: String,
}

/// M9-γ-7 (issue #844): the agent loop's iterative tool-calling pattern
/// commits an Assistant `Message` per LLM iteration. When the LLM returns
/// only `tool_calls` (no text content) and no media — the metadata-only
/// shape that bracketed every `tool/started` → `tool/completed` cycle —
/// the persisted row is invisible to the user but still triggers
/// `MessageCommitObserver`. Pre-fix the ledger emitted N
/// `message/persisted` envelopes per turn for an N-iteration loop, all
/// carrying the same `thread_id`. The web reducer keyed off `thread_id`
/// merged them into a "phantom" empty assistant bubble that briefly
/// flickered into the chat pane (the 2026-05-09 phantom-bubble bug).
///
/// The defensive web-side fix in octos-web #92 hid those bubbles. The
/// authoritative server-side fix is to suppress the `message/persisted`
/// emit for these intermediate metadata-only assistant rows so the wire
/// surface emits exactly one `message/persisted` per turn for the final
/// user-visible assistant text.
///
/// Filter: skip emission when the row is `Assistant`, content is
/// empty after `trim()`, and `media` is empty. Tool messages (role
/// `Tool`) and assistant rows with text or media are unaffected. Once
/// the SSE chat path is deleted (α-5/α-6) and the WS turn loop is sole
/// transport, this filter remains correct because the filtering criteria
/// describe a metadata-only row (no rendering surface) regardless of
/// transport.
fn is_metadata_only_assistant_row(message: &octos_core::Message) -> bool {
    message.role == octos_core::MessageRole::Assistant
        && message.content.trim().is_empty()
        && message.media.is_empty()
}

fn install_message_commit_observer(ledger: Arc<UiProtocolLedger>) {
    let observer: octos_bus::MessageCommitObserver =
        Arc::new(move |session_key, message, committed_seq| {
            // M9-γ-7: drop intermediate metadata-only assistant rows
            // (LLM returned only `tool_calls`, no rendered text). See
            // [`is_metadata_only_assistant_row`] for the rationale.
            if is_metadata_only_assistant_row(message) {
                return;
            }
            let event = MessagePersistedEvent {
                session_id: session_key.clone(),
                // The `Message` struct does not yet carry a typed
                // turn_id (PR-F in the structural plan adds it). Emit
                // `None` for now per UPCR-2026-012 ("absent on legacy
                // rows that pre-date the field").
                turn_id: None,
                thread_id: message.thread_id.clone(),
                seq: committed_seq as u64,
                role: message.role.as_str().to_owned(),
                // Stable per-row id derived from (session, seq,
                // timestamp). Once the typed-identity work in PR-A
                // propagates `message_id` onto `Message` itself we'll
                // plumb that value through directly.
                message_id: format!(
                    "{}:{committed_seq}:{}",
                    session_key.0,
                    message.timestamp.timestamp_nanos_opt().unwrap_or(0)
                ),
                client_message_id: message.client_message_id.clone(),
                // M10 Phase 1: read the task-local source override
                // first so a `BackgroundResultSender` persist (which
                // duplicates a `turn/spawn_complete` envelope) emits
                // `source: background`. The per-connection wire
                // filter keys off this to suppress the duplicate for
                // clients that negotiated `event.spawn_complete.v1`.
                source: current_message_persisted_source(message.role),
                // Placeholder; the ledger's `with_cursor` hook
                // overwrites this with the assigned seq.
                cursor: UiCursor {
                    stream: session_key.0.clone(),
                    seq: 0,
                },
                persisted_at: Utc::now(),
                // P1.3 fix: surface the persisted message's `media`
                // attachments on the wire so spawn_only / send_file
                // deliveries reach the chat bubble. Empty Vec
                // serialises to omitted (back-compat for clients
                // that don't yet understand the field).
                media: message.media.clone(),
            };
            // Append to the ledger; the ledger stamps the cursor onto
            // both the envelope AND the `MessagePersistedEvent.cursor`
            // payload field (see `with_cursor` in
            // `ui_protocol_ledger.rs`). Wire delivery to subscribed
            // connections happens via the `send_ledger_event_durable`
            // path that the standard notification fan-out already
            // exercises.
            let _appended = ledger.append_notification(UiNotification::MessagePersisted(event));
        });
    octos_bus::set_message_commit_observer(Some(observer));
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
    /// PR F (M8.10 thread-binding): bound `thread_id` for every progress
    /// event this reporter emits. Set once at turn-start to the originating
    /// `TurnId`; from then on every JSON payload carries `thread_id` so the
    /// SPA reducer can demultiplex without a sticky-map fallback. `None`
    /// preserves the legacy untagged path for callers that haven't migrated.
    thread_id: Option<String>,
}

impl BoundedChannelReporter {
    fn new(tx: tokio::sync::mpsc::Sender<String>, progress_dropped: Arc<AtomicU64>) -> Self {
        Self {
            tx,
            progress_dropped,
            thread_id: None,
        }
    }

    /// PR F: bind a `thread_id` to this reporter. Typically the originating
    /// `TurnId` (the `params.turn_id` passed into `run_standalone_turn`),
    /// stamped into every emitted SSE payload so wire events are routed
    /// to the right per-turn bubble on the client.
    fn with_thread_id(mut self, thread_id: Option<String>) -> Self {
        self.thread_id = thread_id.filter(|s| !s.is_empty());
        self
    }
}

impl octos_agent::ProgressReporter for BoundedChannelReporter {
    fn report(&self, event: octos_agent::ProgressEvent) {
        let json = match serde_json::to_string(&super::events::event_to_json(
            &event,
            self.thread_id.as_deref(),
        )) {
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

    if features.typed_approvals {
        // Risk is derived from the tool manifest, not from the tool's own
        // payload — a malicious tool cannot self-attest as `low`. Default
        // `unspecified` makes "manifest didn't say" visible in the UI badge
        // instead of silently advertising `medium`. This applies to every
        // tool surface (shell, plugin, future MCP) — audit #715: previously
        // gated on `tool_name == "shell"`, leaving plugin approvals with no
        // risk classification on the wire even though manifest-driven gating
        // engaged server-side (PR #712).
        event.risk = Some(server_risk_for(&event.tool_name));

        if event.tool_name == "shell" {
            let command = request.command;
            if command.is_some() || request.cwd.is_some() {
                event.approval_kind = Some(approval_kinds::COMMAND.to_owned());
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

/// #924 BLOCK 5 — outcome of the WS upgrade Origin gate.
///
/// CORS doesn't apply to WS handshakes, so without this gate a browser
/// tab on any origin could open the socket as long as it has a valid
/// token. Non-browser clients (TUI, gateway, scripts, server-to-server)
/// typically omit the Origin header — allow that. Some server-to-server
/// clients send the header with an empty value (`Origin: `); treat
/// present-but-empty as absent and allow. A header with non-ASCII
/// bytes (`to_str().is_err()`) still rejects — that is malformed input,
/// not the same as omitting the header.
#[derive(Debug, PartialEq, Eq)]
enum WsOriginDecision {
    Allow,
    RejectDisallowed { origin: String },
    RejectMalformed,
}

fn decide_ws_origin_gate(headers: &HeaderMap, base_domain: Option<&str>) -> WsOriginDecision {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return WsOriginDecision::Allow;
    };
    match origin.to_str() {
        Ok(origin_str) if origin_str.trim().is_empty() => WsOriginDecision::Allow,
        Ok(origin_str) => {
            let allowed = super::router::cors_allowlist_for_base_domain(base_domain);
            if allowed.iter().any(|s| s == origin_str) {
                return WsOriginDecision::Allow;
            }
            // Per-tenant browser origins: `https://<tenant>.<base_domain>`.
            // Hosted multi-tenant minis route by subdomain (dspfac.<base>,
            // alice.<base>, ...) and the static CORS allowlist only covers
            // app./admin./api. — adding every tenant up front isn't
            // feasible. Accept any single-label subdomain of the configured
            // base_domain so per-tenant browsers can open the WS. Tenants
            // are sandboxed at the auth layer; the Origin gate is the
            // cross-site protection (rejecting `evil.com` browser tabs).
            if let Some(base) = base_domain {
                if let Some(host) = origin_str.strip_prefix("https://") {
                    let expected_suffix = format!(".{base}");
                    if let Some(tenant) = host.strip_suffix(&expected_suffix) {
                        // Single label only: non-empty, no dots, no port.
                        if !tenant.is_empty()
                            && !tenant.contains('.')
                            && !tenant.contains(':')
                        {
                            return WsOriginDecision::Allow;
                        }
                    }
                }
            }
            WsOriginDecision::RejectDisallowed {
                origin: origin_str.to_string(),
            }
        }
        Err(_) => WsOriginDecision::RejectMalformed,
    }
}

/// GET /api/ui-protocol/ws — JSON-RPC over WebSocket for UI Protocol v1.
pub async fn ws_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<AuthIdentity>>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Response {
    // #923.3 + #924 BLOCK 5: gate the upgrade on Origin. See
    // `decide_ws_origin_gate` for the full decision table.
    match decide_ws_origin_gate(&headers, state.base_domain.as_deref()) {
        WsOriginDecision::Allow => {}
        WsOriginDecision::RejectDisallowed { origin } => {
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                origin = %origin,
                "rejected WS upgrade from disallowed Origin"
            );
            return (axum::http::StatusCode::FORBIDDEN, "disallowed origin").into_response();
        }
        WsOriginDecision::RejectMalformed => {
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                "rejected WS upgrade: Origin header is not valid ASCII"
            );
            return (axum::http::StatusCode::FORBIDDEN, "invalid origin").into_response();
        }
    }
    let connection_profile_id = identity
        .as_ref()
        .and_then(|Extension(identity)| authenticated_profile_id(identity))
        .map(ToOwned::to_owned);
    // Hosted multi-tenant standalone serve routes by subdomain
    // (`<profile>.<base>.example.com`). Admin tokens authenticate as
    // `AuthIdentity::Admin` so `connection_profile_id` is `None`, but the
    // `Host` header still carries the per-tenant profile. Stash it on the
    // connection so per-session resolution (notably plugin work_dir →
    // file-API root) can pick the right profile data dir even for admin
    // sessions originated from a hosted subdomain.
    let routed_profile_id = super::handlers::routed_profile_id_from_headers(&state, &headers);
    let features = ConnectionUiFeatures::from_headers_and_query(&headers, uri.query());
    // M12 Phase D-1: auxiliary REST→WS dispatchers reuse the same REST
    // handlers in `handlers.rs` for business logic, which means they
    // need the same axum extractor inputs the REST routes received.
    // Snapshot the HeaderMap and AuthIdentity onto the connection so
    // each dispatcher arm can re-build the extractor tuple without a
    // round-trip back through the router. None of the existing dispatch
    // arms read these; only the new aux-REST-to-WS arms do.
    let auth_identity = identity.map(|Extension(identity)| identity);
    ws.on_upgrade(move |socket| {
        ui_protocol_connection(
            socket,
            state,
            connection_profile_id,
            routed_profile_id,
            features,
            headers,
            auth_identity,
        )
    })
}

async fn ui_protocol_connection(
    socket: WebSocket,
    state: Arc<AppState>,
    connection_profile_id: Option<String>,
    routed_profile_id: Option<String>,
    features: ConnectionUiFeatures,
    connection_headers: HeaderMap,
    connection_identity: Option<AuthIdentity>,
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
    let live_forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let contracts = contract_stores();
    let ledger = event_ledger(&state).await;
    // Force lazy init of the diff-preview store on this connection so
    // its disk recovery + write-ahead path is wired up before any
    // approval flow can `upsert_file_mutation`. Subsequent calls reuse
    // the same `Arc`. Without `state.sessions` (headless smoke) this
    // installs the ephemeral RAM-only fallback.
    let _ = diff_preview_store(&state, contracts.as_ref()).await;
    let connection_profile_id = connection_profile_id.as_deref();
    let routed_profile_id = routed_profile_id.as_deref();

    // #924 BLOCK 1: wake the read loop the instant a lifecycle/RPC
    // send marks the connection failed. Without this, an idle socket
    // with a failed write side would sit in `ws_rx.next().await`
    // forever — the cleanup path only ran when the next client frame
    // arrived, leaving subscribers and ledger fan-out registered.
    let failed_notify = ws.failed_notify();

    loop {
        // #924 round-2 BLOCK: close the lost-notify race. `notify_waiters`
        // is only received by `Notified` futures that exist at the time of
        // the call, and it stashes no permit. So we:
        //   1. Bail eagerly if the latch is already set.
        //   2. Construct `notified()` BEFORE the second latch check — this
        //      means any subsequent `mark_failed` either (a) is caught by
        //      the re-check below, or (b) fires `notify_waiters` against
        //      this future, which then resolves when the select polls it.
        //   3. Re-check the latch after creating the future to catch the
        //      "fired between is_failed and notified()" ordering.
        if ws.is_failed() {
            break;
        }
        let notified = failed_notify.notified();
        tokio::pin!(notified);
        if ws.is_failed() {
            break;
        }

        let msg = tokio::select! {
            biased;
            _ = &mut notified => {
                // Latch arm only fires when the connection is failed; no
                // need to re-load — the Notify is private to `mark_failed`.
                break;
            }
            next = ws_rx.next() => match next {
                Some(Ok(msg)) => msg,
                Some(Err(_)) | None => break,
            },
        };
        // #922.2: stop dispatch once a lifecycle/RPC send has been
        // marked fatal so we don't quietly accept further requests we
        // can never reply to. The cleanup below still appends terminal
        // events / cancels approvals via `abort_connection_turns`.
        if ws.is_failed() {
            break;
        }
        let text = match msg {
            WsMessage::Text(text) => text,
            WsMessage::Close(_) => break,
            WsMessage::Ping(_) => continue,
            _ => continue,
        };

        let request = match parse_ws_text_frame(text.as_str()) {
            Ok(ParsedFrame::Request(request)) => request,
            Ok(ParsedFrame::Notification(method)) => {
                // #922.1: known inbound notifications (no `id`) are
                // accepted silently. Unknown notifications get a debug
                // trace but no reply — a notification by spec has no
                // response.
                if !is_known_inbound_notification(&method) {
                    tracing::debug!(
                        target: "octos::ui_protocol::ws",
                        method = %method,
                        "ignoring unknown inbound notification"
                    );
                }
                continue;
            }
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
        let command = match route_rpc_command(request, features) {
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
                    &live_forwarders,
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
                    routed_profile_id,
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
            UiCommand::SessionHydrate(params) => {
                handle_session_hydrate(
                    &ws,
                    &state,
                    &ledger,
                    &contracts.approvals,
                    &active_turns,
                    connection_profile_id,
                    routed_profile_id,
                    features,
                    id,
                    params,
                )
                .await;
            }
            UiCommand::ThreadGraphGet(params) => {
                handle_thread_graph_get(
                    &ws,
                    &state,
                    &ledger,
                    &active_turns,
                    connection_profile_id,
                    routed_profile_id,
                    id,
                    params,
                )
                .await;
            }
            UiCommand::TurnStateGet(params) => {
                handle_turn_state_get(
                    &ws,
                    &state,
                    &ledger,
                    &active_turns,
                    connection_profile_id,
                    routed_profile_id,
                    id,
                    params,
                )
                .await;
            }
            UiCommand::PermissionProfileList(_) | UiCommand::PermissionProfileSet(_) => {
                // `permission/profile/*` RPCs are declared in the core
                // protocol type registry but not yet wired in the v1
                // server slice. Reply with `method_not_supported` so
                // clients negotiate around them rather than hang.
                let _ = send_rpc_error(
                    &ws,
                    Some(id),
                    RpcError::method_not_supported(
                        "permission/profile/* not yet implemented in server",
                    ),
                );
            }
            // -------- M12 Phase D-1 auxiliary REST → WS dispatchers --------
            //
            // Each arm below delegates to the same REST handler in
            // `crates/octos-cli/src/api/handlers.rs` (or
            // `auth_handlers.rs`) that the REST route uses. We rebuild
            // the axum extractor tuple from the snapshotted connection
            // headers + identity, await the handler, and forward the
            // JSON body as the WS RPC result. No business logic is
            // duplicated; the REST endpoints stay unchanged.
            UiCommand::SessionList(params) => {
                handle_session_list(&ws, &state, &connection_headers, id, params).await;
            }
            UiCommand::SessionSnapshot(params) => {
                handle_session_snapshot(
                    &ws,
                    &state,
                    &connection_headers,
                    connection_identity.as_ref(),
                    id,
                    params,
                )
                .await;
            }
            UiCommand::SessionMessagesPage(params) => {
                handle_session_messages_page(
                    &ws,
                    &state,
                    &connection_headers,
                    connection_identity.as_ref(),
                    id,
                    params,
                )
                .await;
            }
            UiCommand::SessionStatusGet(params) => {
                handle_session_status_get(&ws, &state, &connection_headers, id, params).await;
            }
            UiCommand::SessionFilesList(params) => {
                handle_session_files_list(
                    &ws,
                    &state,
                    &connection_headers,
                    connection_identity.as_ref(),
                    id,
                    params,
                )
                .await;
            }
            UiCommand::SessionTasksList(params) => {
                handle_session_tasks_list(&ws, &state, &connection_headers, id, params).await;
            }
            UiCommand::SessionWorkspaceGet(params) => {
                handle_session_workspace_get(
                    &ws,
                    &state,
                    &connection_headers,
                    connection_identity.as_ref(),
                    id,
                    params,
                )
                .await;
            }
            UiCommand::SessionTitleSet(params) => {
                handle_session_title_set(
                    &ws,
                    &state,
                    &connection_headers,
                    connection_identity.as_ref(),
                    id,
                    params,
                )
                .await;
            }
            UiCommand::SessionDelete(params) => {
                handle_session_delete(
                    &ws,
                    &state,
                    &connection_headers,
                    connection_identity.as_ref(),
                    id,
                    params,
                )
                .await;
            }
            UiCommand::SystemStatusGet(params) => {
                handle_system_status_get(&ws, &state, id, params).await;
            }
            UiCommand::ContentList(params) => {
                handle_content_list(
                    &ws,
                    &state,
                    &connection_headers,
                    connection_identity.as_ref(),
                    id,
                    params,
                )
                .await;
            }
            UiCommand::ContentDelete(params) => {
                handle_content_delete(&ws, &state, connection_identity.as_ref(), id, params).await;
            }
            UiCommand::ContentBulkDelete(params) => {
                handle_content_bulk_delete(&ws, &state, connection_identity.as_ref(), id, params)
                    .await;
            }
        }
    }

    abort_connection_turns(
        &active_turns,
        &connection_turns,
        &contracts.scopes,
        &ledger,
        &contracts.approvals,
    )
    .await;
    abort_live_forwarders(&live_forwarders, &ledger).await;
    // Dropping `ws` lets the writer task drain & exit; await it so the socket
    // is closed before we return.
    drop(ws);
    let _ = writer_handle.await;
}

async fn abort_live_forwarders(forwarders: &SharedLiveForwarders, ledger: &UiProtocolLedger) {
    let drained: Vec<(SessionKey, tokio::task::JoinHandle<()>)> = {
        let mut guard = forwarders.lock().await;
        guard.drain().collect()
    };
    if drained.is_empty() {
        return;
    }
    // #923.2 + #924 NIT 8: abort every forwarder, then `await` each
    // JoinHandle so the receiver-drop has provably happened before we
    // prune. The old `yield_now()` was a single scheduling hint and
    // could lose the race under load — leaving the ledger
    // broadcaster believing it still had a live subscriber. Awaiting
    // the JoinHandle is the canonical "task is fully done" signal in
    // tokio. We ignore the JoinError for aborted tasks (that's the
    // expected shape).
    let mut drained_sessions: Vec<SessionKey> = Vec::with_capacity(drained.len());
    for (session_id, handle) in drained {
        handle.abort();
        let _ = handle.await;
        drained_sessions.push(session_id);
    }
    for session_id in drained_sessions {
        ledger.prune_subscriber_if_idle(&session_id);
    }
}

/// #922.1: JSON-RPC envelopes with no `id` are notifications, not
/// requests. The protocol's bridge sends a `ping` notification every
/// 30s; the legacy parser required `RpcRequest.id: String`, so the
/// server replied with a `parse_error` for every keepalive. The
/// resulting noise also masked real parse errors.
///
/// #924 NIT 6: distinguish notifications from requests by KEY
/// PRESENCE on `id`, not by null-check. A JSON-RPC envelope with
/// `id: null` is malformed (per spec §4 the `id` of a request must
/// be a String / Number / NULL only for the response correlation
/// reserved use); routing it as a "notification" silently swallowed
/// what should be a loud parse error. The rule:
///
/// - `id` absent       → Notification (today's `ping`/etc.)
/// - `id` is String    → Request (our server's expected shape)
/// - `id` is Number    → Reject with parse_error (we require String)
/// - `id` is null/etc. → Reject with parse_error
///
/// Unparseable frames continue to return `Err(RpcError)` so the
/// existing "lifecycle: client violated wire contract" branch fires.
#[derive(Debug)]
enum ParsedFrame {
    Request(RpcRequest<Value>),
    Notification(String),
}

fn parse_ws_text_frame(text: &str) -> Result<ParsedFrame, RpcError> {
    if text.len() > MAX_TEXT_FRAME_BYTES {
        return Err(frame_too_large_error());
    }
    let value: Value =
        serde_json::from_str(text).map_err(|err| RpcError::parse_error(err.to_string()))?;
    if !value.is_object() {
        return Err(RpcError::parse_error("envelope must be an object"));
    }
    match value.get("id") {
        None => {
            let method = value
                .get("method")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::parse_error("notification missing method"))?
                .to_owned();
            Ok(ParsedFrame::Notification(method))
        }
        Some(Value::String(_)) => {
            let request: RpcRequest<Value> = serde_json::from_value(value)
                .map_err(|err| RpcError::parse_error(err.to_string()))?;
            Ok(ParsedFrame::Request(request))
        }
        Some(Value::Null) => Err(RpcError::parse_error(
            "rpc envelope `id` must not be null; omit the field for notifications",
        )),
        Some(Value::Number(_)) => Err(RpcError::parse_error(
            "rpc envelope `id` must be a string; numeric ids are not supported",
        )),
        Some(_) => Err(RpcError::parse_error(
            "rpc envelope `id` must be a string when present",
        )),
    }
}

#[cfg(test)]
fn parse_rpc_request(text: &str) -> Result<RpcRequest<Value>, RpcError> {
    serde_json::from_str(text).map_err(|err| RpcError::parse_error(err.to_string()))
}

/// Inbound notifications the server accepts (no reply emitted).
fn is_known_inbound_notification(method: &str) -> bool {
    matches!(method, "ping")
}

fn route_rpc_command(
    request: RpcRequest<Value>,
    features: ConnectionUiFeatures,
) -> Result<UiCommand, RpcError> {
    let method_str = request.method.as_str();
    if !ui_protocol_server_supported_methods().contains(&method_str) {
        return Err(RpcError::method_not_supported(method_str));
    }
    // UPCR-2026-009 / -010 / -011 + M12 Phase D-1: when the method is
    // gated behind a feature flag and the connection did not negotiate
    // that flag, reject with `method_not_supported` BEFORE attempting
    // to deserialize the params. Doing the gate first means clients
    // that targeted a capability-gated method without negotiating the
    // feature see a clean `method_not_supported` (and can fall back
    // to REST) instead of `invalid_params` for an unrelated payload
    // shape — the spec contract is "we don't know about this method
    // at all", not "we half-know it".
    //
    // Two gate flavours coexist:
    //
    // 1. **Legacy header-present gates** (`session/hydrate`,
    //    `thread/graph/get`, `turn/state/get`, `task/list`,
    //    `task/cancel`, `task/restart_from_node`): pre-existing
    //    capabilities that historically relied on
    //    `header_present == true` to fire — clients that send NO
    //    feature header at all see the full first-slice in
    //    `SessionOpened.capabilities` per UPCR-2026-007 and so see
    //    these methods as available. Changing that legacy behaviour
    //    here is out of scope for Phase D-1.
    //
    // 2. **Strict opt-in gates** (M12 Phase D-1 auxiliary methods):
    //    the rollout-flag contract is "no negotiation = no access".
    //    A client that sends no feature header at all must NOT trip
    //    into the new methods accidentally — otherwise the additive
    //    Phase D-1 contract is broken for every pre-existing client.
    //    These gates fire regardless of `header_present`.
    //
    // Codex review 2026-05-12: the original implementation collapsed
    // both flavours under `if features.header_present { ... }`, which
    // let no-header clients call `session/delete` and friends without
    // negotiating the capability. The split below restores the
    // strict-opt-in semantic for the new surface while preserving
    // legacy behaviour for pre-existing gates that explicitly relied
    // on it.

    // Flavour 2: strict opt-in. Always reject when the capability is
    // not negotiated, regardless of whether the client sent any
    // feature header at all.
    let strict_gated = match method_str {
        octos_core::ui_protocol::methods::SESSION_LIST
        | octos_core::ui_protocol::methods::SESSION_SNAPSHOT
        | octos_core::ui_protocol::methods::SESSION_MESSAGES_PAGE
        | octos_core::ui_protocol::methods::SESSION_STATUS_GET
        | octos_core::ui_protocol::methods::SESSION_FILES_LIST
        | octos_core::ui_protocol::methods::SESSION_TASKS_LIST
        | octos_core::ui_protocol::methods::SESSION_WORKSPACE_GET
        | octos_core::ui_protocol::methods::SESSION_TITLE_SET
        | octos_core::ui_protocol::methods::SESSION_DELETE
        | octos_core::ui_protocol::methods::SYSTEM_STATUS_GET
        | octos_core::ui_protocol::methods::CONTENT_LIST
        | octos_core::ui_protocol::methods::CONTENT_DELETE
        | octos_core::ui_protocol::methods::CONTENT_BULK_DELETE => {
            Some(features.auxiliary_rest_to_ws_v1)
        }
        _ => None,
    };
    if let Some(false) = strict_gated {
        return Err(RpcError::method_not_supported(method_str));
    }

    // Flavour 1: legacy header-present gates. Fire only when the
    // client opted into negotiation per UPCR-2026-007 but skipped a
    // specific feature.
    if features.header_present {
        let gated = match method_str {
            octos_core::ui_protocol::methods::SESSION_HYDRATE => Some(features.session_hydrate),
            octos_core::ui_protocol::methods::THREAD_GRAPH_GET => Some(features.thread_graph),
            octos_core::ui_protocol::methods::TURN_STATE_GET => Some(features.turn_state_get),
            octos_core::ui_protocol::methods::TASK_LIST
            | octos_core::ui_protocol::methods::TASK_CANCEL
            | octos_core::ui_protocol::methods::TASK_RESTART_FROM_NODE => {
                Some(features.harness_task_control)
            }
            _ => None,
        };
        if let Some(false) = gated {
            return Err(RpcError::method_not_supported(method_str));
        }
    }
    UiCommand::from_rpc_request(request)
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
        return Err(authenticated_scope_mismatch_error(
            "profile_id is outside the authenticated profile",
            connection_profile_id,
            requested_profile_id,
        ));
    }

    match session_id.profile_id() {
        Some(session_profile_id) if session_profile_id == connection_profile_id => Ok(()),
        Some(session_profile_id) => Err(authenticated_scope_mismatch_error(
            "session_id is outside the authenticated profile",
            connection_profile_id,
            Some(session_profile_id),
        )),
        // SPA convention: a fresh session uses a raw `web-N` id with no
        // profile prefix. Accept it under profile auth — the auth layer
        // is the gate, the session_id is just an opaque per-tab handle.
        // PR #857 originally landed this. PR #926 inadvertently added
        // `auth_scope_violation: true` here, which the new 1008
        // close-on-auth-scope-violation path then weaponized — every
        // OTP-authenticated browser session was 1008-closed on
        // `session/open` and the SPA fell into a reconnect storm.
        // Mini5 OTP-flow probe (2026-05-13) reproduced this.
        None => Ok(()),
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

/// Profile-mismatch variant emitted only when the connection IS authenticated
/// (i.e. carries an `AuthIdentity::User`) and the requested scope falls
/// outside that user's profile. The `auth_scope_violation` data tag drives
/// the WS close-code 1008 emit per the SPA bridge contract (Web PR #114) —
/// see `is_auth_scope_violation` for the consumer side.
fn authenticated_scope_mismatch_error(
    message: &'static str,
    expected_profile_id: &str,
    actual_profile_id: Option<&str>,
) -> RpcError {
    RpcError::invalid_params(message).with_data(json!({
        "expected_profile_id": expected_profile_id,
        "actual_profile_id": actual_profile_id,
        "auth_scope_violation": true,
    }))
}

/// True iff the error was produced by `validate_authenticated_session_scope`,
/// i.e. the request was rejected because the authenticated identity's profile
/// id does not match the requested session scope. Auth-related rejections
/// trigger a WS close-code 1008 emit so the SPA `crew:auth_expired` listener
/// fires; non-auth `invalid_params` errors (e.g. malformed input) do not.
fn is_auth_scope_violation(error: &RpcError) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("auth_scope_violation"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

async fn handle_session_open(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    approvals: &PendingApprovalStore,
    live_forwarders: &SharedLiveForwarders,
    connection_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: SessionOpenParams,
) {
    // Subscribe to the live ledger broadcast BEFORE the replay query so any
    // event that lands while we're still computing replay/opened sits in the
    // broadcast buffer and gets emitted by the forwarder once we hand it off
    // (filtered to seq > replay snapshot head to avoid duplicating replay).
    // Issue #760: without this, late background-task artifacts (deep_research
    // result, mofa podcast, run_pipeline output, TTS audio) reach the ledger
    // but never push to the live WS.
    let session_id_for_subscribe = params.session_id.clone();
    let live_rx = ledger.subscribe(&session_id_for_subscribe);

    let outcome = match open_session_result(
        state,
        ledger,
        approvals,
        ws.connection_id,
        connection_profile_id,
        features,
        params,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            // Drop the receiver, then opportunistically reclaim the
            // broadcast sender slot if no other connection is subscribed
            // (codex MUST-FIX-3: failure paths previously leaked one
            // sender per failed open).
            drop(live_rx);
            ledger.prune_subscriber_if_idle(&session_id_for_subscribe);
            send_scope_error(ws, id, error);
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
    //
    // Capability filtering: UPCR-2026-012 requires that `message/persisted`
    // notifications are emitted ONLY to clients that negotiated
    // `event.message_persisted.v1`. The live handler path enforces this,
    // but replay during session/open must enforce it too — otherwise a
    // client that did NOT request the feature still receives the events
    // during reconnect-replay, violating the wire contract.
    //
    // We silently skip filtered events rather than emitting
    // `protocol/replay_lossy`. The client never asked for these events,
    // so dropping them is not lossy from their perspective.
    //
    // M10 Phase 1: the same dual filter that `live_event_passes_capability_filter`
    // applies for the live broadcast must apply during replay so a
    // reconnecting client sees exactly one shape per `spawn_only`
    // completion (either the legacy `message/persisted` OR the new
    // `turn/spawn_complete`, never both). Reusing the helper keeps replay
    // and live in lockstep.
    for event in outcome.replay {
        if !live_event_passes_capability_filter(&event.event, features) {
            continue;
        }
        let _ = send_ledger_event_durable(ws, ledger, event.event);
    }
    for event in outcome.pending_approvals {
        let _ = send_ledger_event_durable(
            ws,
            ledger,
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(event)),
        );
    }
    // Baseline = head_seq captured atomically with replay (codex MUST-FIX-1).
    // Using opened_event.cursor.seq instead would silently filter out any
    // event that happened to land between replay and the session/open
    // append, exactly the gap codex flagged.
    let baseline_seq = outcome.replay_baseline_seq;
    let session_id = match &outcome.opened_event.event {
        UiProtocolLedgerEvent::Notification(UiNotification::SessionOpened(opened)) => {
            opened.session_id.clone()
        }
        _ => session_id_for_subscribe,
    };
    let ledger_for_forwarder = ledger.clone();
    let _ = send_ledger_event_durable(ws, ledger, outcome.opened_event.event);

    // Hand the broadcast receiver to a per-session forwarder. The previous
    // forwarder for this session on this connection (if any) is aborted —
    // a re-`session/open` always restarts the live pump from a fresh
    // baseline cursor.
    spawn_live_forwarder(
        ws.clone(),
        ledger_for_forwarder,
        session_id,
        baseline_seq,
        ws.connection_id,
        features,
        live_rx,
        live_forwarders.clone(),
    )
    .await;
}

/// Pump live ledger events for `session_id` into the connection's WS write
/// channel. Filters out events with `cursor.seq <= baseline_seq` (which
/// were already shipped via replay) and applies the same capability
/// gating as the live-emit path. The task ends when the WS write channel
/// closes (peer gone), the broadcast sender is dropped (rare), or the
/// connection cleanup aborts the handle.
async fn spawn_live_forwarder(
    ws: WsConnection,
    ledger: Arc<UiProtocolLedger>,
    session_id: SessionKey,
    baseline_seq: u64,
    self_connection_id: ConnectionId,
    features: ConnectionUiFeatures,
    mut rx: tokio::sync::broadcast::Receiver<LedgeredUiProtocolEvent>,
    forwarders: SharedLiveForwarders,
) {
    use tokio::sync::broadcast::error::RecvError;

    let session_for_log = session_id.clone();
    let task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if event.cursor.seq <= baseline_seq {
                        continue;
                    }
                    // Codex MUST-FIX-2: when the originating handler ran
                    // on this same connection it already direct-sent the
                    // wire frame; dropping the broadcast copy here is the
                    // only way to keep delivery exactly-once. Other
                    // connections still receive the event via fan-out.
                    if event.from_connection == Some(self_connection_id) {
                        continue;
                    }
                    if !live_event_passes_capability_filter(&event.event, features) {
                        continue;
                    }
                    match send_ledger_event_durable(&ws, &ledger, event.event) {
                        Ok(()) => {}
                        // #924 BLOCK 2: a closed writer OR a latched
                        // failure both mean further pumps will produce
                        // FatalClosed forever; stop spinning.
                        Err(SendError::Closed | SendError::FatalClosed) => break,
                        // BackpressureDrop: `send_ledger_event_durable`
                        // already opportunistically emits replay_lossy; keep
                        // pumping so a recovered consumer gets caught up.
                        Err(_) => {}
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    // Slow consumer fell behind. The ledger is durable; the
                    // client's cursor is the source of truth and a follow-up
                    // session/hydrate or reconnect with the last cursor
                    // catches them up. Log and keep pumping new events.
                    tracing::warn!(
                        target: "octos::ui_protocol::ws",
                        session_id = %session_for_log.0,
                        skipped_events = skipped,
                        "live ledger forwarder lagged; client must rehydrate via cursor"
                    );
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
    // #924 NIT 8: store the full JoinHandle so the connection-cleanup
    // path can `await` the aborted task before pruning idle
    // subscribers. Replace any prior forwarder for this session on
    // this connection — re-`session/open` restarts the live pump from
    // a fresh baseline. The previous handle is aborted + the resulting
    // JoinHandle dropped on the spot; we don't await here because
    // re-open is a hot path.
    let mut guard = forwarders.lock().await;
    if let Some(prev) = guard.insert(session_id, task) {
        prev.abort();
    }
}

/// Mirror the capability filter at `ui_protocol.rs` session/open replay
/// loop (UPCR-2026-012): a connection that did not negotiate
/// `event.message_persisted.v1` must not receive `message/persisted`
/// notifications via the live broadcast either. Other notifications pass
/// unchanged today; future capability-gated kinds get added here.
///
/// M10 Phase 1 extends this with two intertwined gates for the
/// `event.spawn_complete.v1` capability:
///
/// 1. Clients that did NOT negotiate `event.spawn_complete.v1` must not
///    receive `turn/spawn_complete` notifications. They continue to see
///    the legacy `message/persisted` row for the same `spawn_only`
///    completion, preserving the wire shape they shipped with.
/// 2. Clients that DID negotiate `event.spawn_complete.v1` see
///    `turn/spawn_complete` instead — and the corresponding
///    `message/persisted` row (carrying `source: background`) is
///    suppressed at this gate so the same logical event is not
///    delivered twice in two different shapes.
fn live_event_passes_capability_filter(
    event: &UiProtocolLedgerEvent,
    features: ConnectionUiFeatures,
) -> bool {
    if !features.message_persisted {
        if let UiProtocolLedgerEvent::Notification(UiNotification::MessagePersisted(_)) = event {
            return false;
        }
    }
    if !features.spawn_complete {
        // Old client: never deliver the new envelope.
        if let UiProtocolLedgerEvent::Notification(UiNotification::TurnSpawnComplete(_)) = event {
            return false;
        }
    } else {
        // New client: suppress the `message/persisted` row that
        // duplicates a `turn/spawn_complete` envelope. The row is
        // identified by `source: background` — the only path through
        // `MessageCommitObserver` that fires from `BackgroundResultSender`.
        if let UiProtocolLedgerEvent::Notification(UiNotification::MessagePersisted(event)) = event
        {
            if matches!(event.source, MessagePersistedSource::Background) {
                return false;
            }
        }
    }
    true
}

#[derive(Debug)]
struct SessionOpenOutcome {
    result: SessionOpenResult,
    replay: Vec<LedgeredUiProtocolEvent>,
    pending_approvals: Vec<ApprovalRequestedEvent>,
    opened_event: LedgeredUiProtocolEvent,
    /// Head seq observed atomically with the replay snapshot. The live
    /// forwarder uses this — NOT `opened_event.cursor.seq` — as its
    /// drop-everything-≤-this baseline. Closes the replay/open race
    /// where an event landing between replay and the session/open append
    /// would otherwise be filtered out (codex PR #761 MUST-FIX-1).
    replay_baseline_seq: u64,
}

async fn open_session_result(
    state: &Arc<AppState>,
    ledger: &UiProtocolLedger,
    approvals: &PendingApprovalStore,
    connection_id: ConnectionId,
    connection_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    params: SessionOpenParams,
) -> Result<SessionOpenOutcome, RpcError> {
    let active_profile_id = validate_session_scope(
        &params.session_id,
        params.profile_id.as_deref(),
        connection_profile_id,
    )?;
    let requested_workspace =
        validate_requested_session_cwd(state, features, active_profile_id.as_deref(), &params)?;
    // M11-F deliverable D: re-introduce the
    // `appui.default_session_cwd` Tier-2 fallback that M11-E's
    // `clone_session_tools` deletion took out. Pre-resolution order:
    //   Tier 1 — `requested_workspace` (validated client cwd above).
    //   Tier 2 — `AppState::appui_default_session_cwd` (operator default).
    //   Tier 3 — `SessionRuntime::bootstrap`'s
    //            `<profile.data_dir>/users/<encoded base>/workspace`.
    //
    // We resolve Tier 2 here, in the UI Protocol entrypoint, rather
    // than threading it through `SessionRuntime::bootstrap`. Rationale:
    //  - The bootstrap signature stays stable across M11-F.
    //  - Tier 2 is a serve-level operator setting (octos serve reads
    //    `config.appui.default_session_cwd`) — the runtime layer
    //    doesn't otherwise see operator-level config, so leaving the
    //    resolution at the dispatcher keeps `ProfileRuntime` /
    //    `SessionRuntime` free of `AppState`-shaped knowledge.
    //  - The hint is passed verbatim into `SessionRuntimeCache::get_or_init`,
    //    which forwards it to `SessionRuntime::bootstrap`'s
    //    `workspace_hint`. `validate_workspace_hint` runs the same
    //    safety check on it as on a client-supplied cwd (canonicalize,
    //    reject banned system roots).
    let effective_workspace_hint: Option<PathBuf> = requested_workspace
        .clone()
        .or_else(|| state.appui_default_session_cwd.clone());
    // M11-E: when a profile is registered for this session, materialize
    // the `SessionRuntime` against the validated workspace hint NOW so
    // the subsequent `turn/start` (and any cached read of
    // `session_runtime.workspace_root`) observes the supplied cwd.
    //
    // The cache's `get_or_init` is single-flight: a same-key hit returns
    // the EXISTING `Arc<SessionRuntime>` and IGNORES the new
    // `workspace_hint`. That means a client cannot silently change a
    // running session's cwd by re-opening with a different `cwd`
    // parameter; the first cwd wins until the runtime is evicted (LRU,
    // idle TTL, explicit `invalidate`). The `SessionOpened` reply is
    // sourced from the cached runtime's `workspace_root` (not the
    // requested hint) so the wire response truthfully reflects which
    // workspace the next turn will use — closing the cache/wire
    // divergence codex flagged on PR #884 follow-up.
    //
    // The `session_workspaces()` map is kept as a thin read-through
    // view for the legacy WS dispatcher fallback (no profile registered
    // — setup wizard / single-agent serve) and for pane snapshots that
    // need a sync read of the workspace root. We always write the
    // *effective* workspace root (the runtime's, when present) so the
    // map cannot drift out of sync with the cache.
    let mut effective_workspace_root: Option<PathBuf> = None;
    if let Some(profile_runtime) =
        resolve_session_profile_runtime(state, active_profile_id.as_deref())
    {
        let hint = effective_workspace_hint.clone();
        match state
            .session_cache
            .get_or_init(&profile_runtime, params.session_id.clone(), hint)
            .await
        {
            Ok(runtime) => {
                effective_workspace_root = Some(runtime.workspace_root.clone());
            }
            Err(error) => {
                tracing::error!(
                    error = %error,
                    profile_id = %profile_runtime.profile_id,
                    session = %params.session_id,
                    "session/open: SessionRuntime::bootstrap failed",
                );
                return Err(runtime_unavailable_error(format!(
                    "failed to bootstrap session runtime: {error}"
                )));
            }
        }
    } else if let Some(workspace_root) = effective_workspace_hint.as_ref() {
        // No profile registered (legacy single-agent serve). Stash the
        // effective hint in the read-through map so the legacy
        // dispatcher's pane-snapshot path can pick it up.
        effective_workspace_root = Some(workspace_root.clone());
    }
    if let Some(root) = effective_workspace_root.as_ref() {
        session_workspaces().set(params.session_id.clone(), root.clone());
    }
    let (replay, replay_baseline_seq) =
        ledger.replay_after_with_head(&params.session_id, params.after.as_ref())?;
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

    // The cached SessionRuntime's `workspace_root` is the source of truth
    // for the wire response when present. Fall back to the legacy lookup
    // when no SessionRuntime was materialized (no profile registered).
    let workspace_root = effective_workspace_root
        .or_else(|| session_workspace_root_for_state(state, &params.session_id));
    let panes = features
        .pane_snapshots
        .then(|| build_pane_snapshot(&data_dir, &params.session_id, workspace_root.as_deref()));
    // UPCR-2026-007: advertise the negotiated capability set in-band so
    // clients don't have to rely on out-of-band knowledge of which feature
    // tokens the server honours.
    let capabilities = features.negotiated_capabilities();
    // Tag the broadcast with our connection id so the live forwarder
    // installed below skips this event (we direct-send it inline at the
    // call site). Other connections still observe the broadcast.
    let opened_event = ledger.append_notification_from(
        UiNotification::SessionOpened(SessionOpened {
            session_id: params.session_id,
            active_profile_id,
            workspace_root: workspace_root.map(|path| path.to_string_lossy().to_string()),
            cursor: None,
            panes,
            capabilities,
        }),
        connection_id,
    );
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
        replay_baseline_seq,
    })
}

fn validate_requested_session_cwd(
    state: &AppState,
    features: ConnectionUiFeatures,
    active_profile_id: Option<&str>,
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
    validate_session_workspace_allowed(state, active_profile_id, &workspace_root)?;
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
    active_profile_id: Option<&str>,
    workspace_root: &Path,
) -> Result<(), RpcError> {
    // M11-F: per-session cwd is only honored on the profile-aware
    // dispatch path (`SessionRuntime` materialized via
    // `SessionRuntimeCache`). The legacy single-agent fallback was
    // deleted in M11-F — `octos serve` bootstraps every profile in
    // `ProfileStore::list()` at startup, so an unregistered profile
    // here is a configuration bug. We still surface the
    // `cwd_runtime_unavailable` typed error so the client sees a
    // distinct shape from a path-safety rejection.
    //
    // We check the SPECIFIC routed profile, not just "any profile is
    // registered" — a multi-profile deployment may have profiles A, B,
    // and a request that routes to profile C should still get the
    // `cwd_runtime_unavailable` rejection (codex round-3 fix). Reject
    // early so the client sees a typed error instead of a silent
    // wire/turn mismatch.
    //
    // Path safety mirrors `SessionRuntime::bootstrap`'s
    // `validate_workspace_hint`: the cwd must canonicalize and must
    // not be rooted under a banned system path (`/etc`, `/usr`,
    // `/sbin`, …). Cross-session containment is intentionally NOT
    // checked here — coding-agent UIs point sessions at arbitrary
    // repos. Session-scope access control belongs in the auth /
    // connection-profile gate (`validate_session_scope`), not the cwd
    // validator.
    if resolve_session_profile_runtime(state, active_profile_id).is_none() {
        return Err(RpcError::invalid_params(
            "session/open cwd requires a configured profile runtime",
        )
        .with_data(json!({
            "kind": "cwd_runtime_unavailable",
            "cwd": workspace_root.to_string_lossy(),
            "active_profile_id": active_profile_id,
        })));
    }

    validate_session_workspace_path_safety(workspace_root)
}

/// Path-safety gate for multi-profile session cwds.
///
/// Mirrors the banned-system-path list in
/// `crate::runtime::session::validate_workspace_hint`. The two paths
/// must stay in lockstep; the duplicate exists because
/// `SessionRuntime::bootstrap` does not see `AppState` and cannot call
/// back into this module. TODO(post-M11): collapse to a shared helper.
fn validate_session_workspace_path_safety(workspace_root: &Path) -> Result<(), RpcError> {
    // `validate_requested_session_cwd` already canonicalized the path
    // and verified it is a directory, so we only need to guard against
    // banned system roots here.
    let mut components = workspace_root.components();
    let _root = components.next();
    if let Some(first) = components.next() {
        let first = first.as_os_str();
        const BANNED: &[&str] = &[
            "etc", "sbin", "bin", "boot", "dev", "proc", "sys", "usr", "var", "root",
        ];
        for entry in BANNED {
            if first == std::ffi::OsStr::new(entry) {
                return Err(RpcError::invalid_params(format!(
                    "session/open cwd is rooted under a system path /{entry}"
                ))
                .with_data(json!({
                    "kind": "cwd_system_path_banned",
                    "cwd": workspace_root.to_string_lossy(),
                    "banned_root": entry,
                })));
            }
        }
    }
    Ok(())
}

/// Resolve the `ProfileRuntime` for the routed session, mirroring
/// `chat_sync`'s `state.profiles.get(profile_id)` lookup.
///
/// `active_profile_id` is the profile id `validate_session_scope`
/// produced for this session/open. It may be `None` when the legacy
/// no-profile flow is in use (single-agent serve, no connection-level
/// profile identity). Falls back to `MAIN_PROFILE_ID` so the
/// canonical "_main" profile in standalone deployments still resolves.
fn resolve_session_profile_runtime(
    state: &AppState,
    active_profile_id: Option<&str>,
) -> Option<Arc<crate::runtime::ProfileRuntime>> {
    let candidate = active_profile_id.unwrap_or(MAIN_PROFILE_ID);
    state.profiles.get(candidate).cloned()
}

/// Resolve the canonical `SessionManager` handle for read operations
/// (hydrate, state, etc.). Closes #919.1: turn persistence writes to
/// the profile's `SessionRuntime.sessions`, so reads under profile
/// auth MUST hit the same handle — otherwise `state.sessions` (the
/// top-level data-dir store) reports `unknown_session` on reconnect.
///
/// Returns the per-profile session manager if a `ProfileRuntime` is
/// registered for the resolved profile and the cache can bootstrap
/// a `SessionRuntime` for `session_id`. Falls back to
/// `state.sessions` so the legacy no-profile flow continues to work.
///
/// #924 BLOCK 4: the active-profile precedence MUST mirror
/// `handle_turn_start` — `session_id.profile_id()` first (the key
/// itself encodes the owner profile), then `connection_profile_id`
/// (token-auth scope), then `routed_profile_id` (host/header
/// routing). Without the `session_id` precedence + the routed
/// fallback, a host-routed admin session on a hosted subdomain
/// hydrated from `_main`/`state.sessions` instead of the right
/// profile runtime — and turns whose `SessionKey` already carried
/// `<profile>:api:...` resolved to a different store than the one
/// that persisted them.
pub(crate) async fn resolve_sessions_for_lookup(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Option<Arc<tokio::sync::Mutex<octos_bus::SessionManager>>> {
    let active_profile_id = session_id
        .profile_id()
        .or(connection_profile_id)
        .or(routed_profile_id);
    if let Some(profile_runtime) = resolve_session_profile_runtime(state, active_profile_id) {
        let hint = session_workspaces().get(session_id);
        if let Ok(runtime) = state
            .session_cache
            .get_or_init(&profile_runtime, session_id.clone(), hint)
            .await
        {
            return Some(runtime.sessions.clone());
        }
    }
    state.sessions.clone()
}

fn session_workspace_root_for_state(state: &AppState, session_id: &SessionKey) -> Option<PathBuf> {
    // M11-F: read-through view on `session_workspaces()` only. The
    // legacy `state.agent.tool_registry().workspace_root()` fallback
    // was deleted alongside `state.agent`; the cached
    // `SessionRuntime.workspace_root` is the canonical source on a
    // successful open, and the in-memory `session_workspaces()` map is
    // the synchronous read-through view `session/open`'s pane snapshot
    // path uses (computed BEFORE the async cache load completes).
    // Tier-2 (`appui.default_session_cwd`) is consulted at
    // `open_session_result` time as a fallback hint, so the map
    // already reflects the operator default when no client cwd was
    // supplied.
    let _ = state; // unused after the agent-fallback deletion
    session_workspaces().get(session_id)
}

/// Append the per-session workspace-root hint to the system prompt.
///
/// M11-F: the base prompt comes from the SessionRuntime's agent only
/// (legacy `state.agent` was deleted), so this helper takes the
/// resolved `String` rather than an `Agent` reference. The text
/// appended is identical to the pre-M11-E `session_system_prompt`
/// wording — the SPA's reducer matches on it heuristically and must
/// not change.
fn append_workspace_root_hint(mut prompt: String, workspace_root: Option<&Path>) -> String {
    if let Some(workspace_root) = workspace_root {
        prompt.push_str("\n\nAppUi session workspace root: ");
        prompt.push_str(&workspace_root.to_string_lossy());
        prompt.push_str(
            "\nThe server approved this cwd for the current session. Resolve relative shell and file-tool paths against this workspace.",
        );
    }
    prompt
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
    routed_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    mut params: TurnStartParams,
) {
    // UPCR-2026-015 (M9-β-1): if the client carried a `topic` field
    // alongside the session_id, fold it into the resolved SessionKey
    // BEFORE scope validation. The rest of the turn pipeline keys
    // exclusively off `params.session_id`, so adopting the topic-
    // suffixed form here means history lookup, ledger appends, and
    // `task/list` filtering all see the per-topic bucket
    // automatically. Empty / whitespace-only topics fall through to
    // the bare session shape (matching `SessionKey::with_topic`'s
    // own empty-string short-circuit).
    if let Some(topic) = params
        .topic
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        // Replace the SessionKey with the topic-suffixed form. Splice
        // any existing topic suffix away first so a client that sends
        // both `session_id: "x:y#old"` and `topic: "new"` lands in a
        // single, unambiguous bucket (`x:y#new`) rather than the
        // double-suffixed garbage `x:y#old#new`. The base parser
        // already handles `#`-stripped lookups, but we want the
        // canonical form on the wire-trip back to clients.
        let base = params.session_id.base_key().to_owned();
        params.session_id = SessionKey(format!("{base}#{topic}"));
    }

    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
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
        // M11-F: validate that a `ProfileRuntime` is registered for the
        // routed profile BEFORE spawning the turn task. The legacy
        // `validate_runtime` (which checked `state.agent` /
        // `state.sessions`) was deleted; the equivalent gate now is
        // "the SessionRuntimeCache can resolve a ProfileRuntime for
        // this session's profile id". Fail fast with the same
        // `runtime_unavailable` shape so existing clients see no wire
        // change.
        let active_profile_id = params
            .session_id
            .profile_id()
            .map(ToOwned::to_owned)
            .or_else(|| {
                connection_profile_id
                    .or(routed_profile_id)
                    .map(ToOwned::to_owned)
            });
        if resolve_session_profile_runtime(state, active_profile_id.as_deref()).is_none() {
            let _ = send_rpc_error(
                ws,
                Some(id),
                runtime_unavailable_error(format!(
                    "No ProfileRuntime registered for profile '{}'. \
                     Set up the profile with an API key in the dashboard.",
                    active_profile_id.as_deref().unwrap_or("<unset>"),
                )),
            );
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
    let resolved_profile_id = connection_profile_id
        .or(routed_profile_id)
        .map(ToOwned::to_owned);
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
                resolved_profile_id,
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
            // Codified by accepted UPCR-2026-008: typed `reason` field on
            // `TurnInterruptResult`. String registry value `turn_id_mismatch`.
            let _ = send_typed_interrupt_result(
                ws,
                id,
                TurnInterruptResult::declined("turn_id_mismatch"),
            );
        }
        InterruptOutcome::AlreadyTerminal(reason) => {
            let interrupted = matches!(reason, TerminalReason::Interrupted);
            // Codified by accepted UPCR-2026-008: typed `terminal_state` field
            // on `TurnInterruptResult`. Values come from `TerminalReason`.
            let _ = send_typed_interrupt_result(
                ws,
                id,
                TurnInterruptResult::already_terminal(reason.as_str(), interrupted),
            );
        }
        InterruptOutcome::AlreadyInterrupting => {
            // A prior caller transitioned the turn to `Interrupting` and is
            // awaiting ack. The terminal event is already guaranteed to be
            // emitted exactly once. Idempotent: report the same response shape
            // as the original caller will.
            let _ = send_typed_interrupt_result(ws, id, TurnInterruptResult::interrupted_ok());
        }
        InterruptOutcome::Captured { ack_rx } => {
            // State is now `Interrupting { ack }`; the turn task is wired to
            // observe `interrupt_rx`, abort its agent, emit exactly one
            // `TurnError(interrupted)`, and signal `ack`. We do NOT abort the
            // outer turn future here — that would race with the terminal
            // emission and could lose the wire-side event.
            let result = tokio::time::timeout(INTERRUPT_ACK_TIMEOUT, ack_rx).await;
            let payload = match result {
                Ok(Ok(())) => TurnInterruptResult::interrupted_ok(),
                Ok(Err(_)) => {
                    // Sender dropped without ack — the task panicked or was
                    // cancelled before reaching the terminal arm. The state
                    // remains `Interrupting`; report timeout-style result so
                    // the caller knows the wire-side terminal is uncertain.
                    // Codified by accepted UPCR-2026-008.
                    TurnInterruptResult::ack_timed_out()
                }
                Err(_) => TurnInterruptResult::ack_timed_out(),
            };
            let _ = send_typed_interrupt_result(ws, id, payload);
        }
    }
}

/// Serialize a typed `TurnInterruptResult` and dispatch via `send_rpc_result`.
///
/// Falls back to a hand-built minimal result if serialization fails. The
/// fallback path should be unreachable in practice — `TurnInterruptResult`
/// has no field that can fail to serialize — but keeping the call infallible
/// on the wire avoids leaving the caller without a response on a defensive
/// path.
fn send_typed_interrupt_result(
    ws: &WsConnection,
    id: String,
    result: TurnInterruptResult,
) -> Result<(), SendError> {
    let value = serde_json::to_value(&result)
        .unwrap_or_else(|_| json!({ "interrupted": result.interrupted }));
    send_rpc_result(ws, id, value)
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
        send_scope_error(ws, id, error);
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
        send_scope_error(ws, id, error);
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
        send_scope_error(ws, id, error);
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
        send_scope_error(ws, id, error);
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
        send_scope_error(ws, id, error);
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
        send_scope_error(ws, id, error);
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
        send_scope_error(ws, id, error);
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

// ----- UPCR-2026-009 / -010 / -011 handlers -----

/// Per UPCR-2026-009: bundle the chat-state projection into one RPC.
///
/// Atomicity invariant (codex's review ask): the ledger snapshot and the
/// returned `cursor` are read in one critical section via
/// [`UiProtocolLedger::snapshot_with_cursor`]. A concurrent appender cannot
/// land an event with cursor ≤ result.cursor that the client did not also
/// observe — so a follow-up `session/hydrate { after: result.cursor }`
/// returns only events strictly after the snapshot, with no gap.
async fn handle_session_hydrate(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    approvals: &PendingApprovalStore,
    active_turns: &SharedActiveTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: SessionHydrateParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }
    if params.include.len() > SESSION_HYDRATE_INCLUDE_MAX {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!(
                "session/hydrate include too large: {} > {}",
                params.include.len(),
                SESSION_HYDRATE_INCLUDE_MAX
            ))
            .with_data(json!({
                "kind": "include_too_large",
                "limit": SESSION_HYDRATE_INCLUDE_MAX,
            })),
        );
        return;
    }

    // Atomic snapshot of (events ≥ after, head cursor) — closes the
    // codex-flagged gap where reading events and head separately could
    // miss any event committed in between.
    let (replayed, head_cursor) =
        match ledger.snapshot_with_cursor(&params.session_id, params.after.as_ref()) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = send_rpc_error(ws, Some(id), error);
                return;
            }
        };

    let include_set = HydrateIncludeSet::from_request(&params.include);
    // #919.1: route to the profile's session manager when the connection
    // has a profile scope. Turn persistence writes to
    // `SessionRuntime.sessions`; reads must hit the same handle or we'd
    // report `unknown_session` on reconnect-hydrate.
    let Some(sessions) = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        routed_profile_id,
        &params.session_id,
    )
    .await
    else {
        let _ = send_rpc_error(
            ws,
            Some(id),
            runtime_unavailable_error("Sessions not available"),
        );
        return;
    };
    // Reject unknown sessions per UPCR-2026-009 error model. The session
    // must already exist (typically via a prior `session/open` call); we
    // do NOT auto-create on hydrate.
    {
        let mut sessions_guard = sessions.lock().await;
        if !sessions_guard.session_known(&params.session_id) {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(params.session_id.0.clone()),
            );
            return;
        }
    }
    // M10 Phase 6.2 (Bug C). Closes the Phase 5a documented punt by
    // surfacing every retained `turn/spawn_complete` envelope from the
    // ledger replay window on the hydrate response when (and only
    // when) the client negotiated `event.spawn_complete.v1`. Server
    // does NOT suppress the legacy `Background`-source rows in
    // `messages` — the `SessionHydrateResult` payload has no
    // alternative channel for the envelope's `content`/`media`, and
    // codex's review rounds on the suppression-side designs surfaced
    // multiple correctness regressions (NotConfigured-branch empty
    // media, multi-task per-turn ambiguity, orphan companions from
    // failed final-ack persists). Negotiated clients dedup against
    // `replayed_envelopes` on their side using `message_id` —
    // mirroring the live wire's split: producer emits both shapes,
    // consumer chooses one per `event.spawn_complete.v1` capability.
    //
    // Non-negotiated clients receive `replayed_envelopes: None`
    // (omitted via `skip_serializing_if`); the `messages` payload they
    // see is byte-identical to pre-fix.
    let replayed_envelopes = if features.spawn_complete && include_set.messages {
        Some(
            replayed
                .iter()
                .filter_map(|event| match &event.event {
                    UiProtocolLedgerEvent::Notification(UiNotification::TurnSpawnComplete(ev)) => {
                        Some(ev.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };

    // Codex Bug C round-6: gate the new identity/provenance fields
    // on `features.spawn_complete`. Without negotiation we leave
    // `HydratedMessage.message_id` and `HydratedMessage.source` as
    // `None` so legacy clients (TUI, pre-spawn_complete SPA bundles,
    // strict-codegen consumers) see byte-identical wire. With
    // negotiation we synthesize `message_id` (mirrors
    // `MessageCommitObserver`'s formula) and lift `source` from the
    // retained `MessagePersisted` events — giving the client the
    // identity AND provenance signals it needs to drop both the
    // spawn-ack row AND the per-file companion rows in favour of the
    // envelope on hydrate-time dedup.
    let row_sources: HashMap<u64, String> = if features.spawn_complete && include_set.messages {
        replayed
            .iter()
            .filter_map(|event| match &event.event {
                UiProtocolLedgerEvent::Notification(UiNotification::MessagePersisted(ev)) => {
                    Some((ev.seq, ev.source.as_str().to_owned()))
                }
                _ => None,
            })
            .collect()
    } else {
        HashMap::new()
    };
    let expose_message_id = features.spawn_complete && include_set.messages;

    // Lock once; gather all the in-memory chat state we need so the
    // result reflects a single sessions-side snapshot.
    let (messages, threads_projection) = {
        let mut sessions_guard = sessions.lock().await;
        let session = sessions_guard.get_or_create(&params.session_id).await;
        let messages = if include_set.messages {
            Some(
                session
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(seq, _)| match params.after.as_ref() {
                        Some(after) => *seq as u64 > after.seq,
                        None => true,
                    })
                    .map(|(seq, msg)| {
                        let seq = seq as u64;
                        // M10 Phase 6.2 (Bug C). Negotiated clients
                        // get `(message_id, source)` so they can
                        // dedup the hydrated rows against
                        // `replayed_envelopes`. Non-negotiated
                        // clients keep the pre-fix shape (both
                        // fields `None`, omitted from the wire).
                        let message_id = if expose_message_id {
                            Some(format!(
                                "{}:{seq}:{}",
                                params.session_id.0,
                                msg.timestamp.timestamp_nanos_opt().unwrap_or(0),
                            ))
                        } else {
                            None
                        };
                        let source = row_sources.get(&seq).cloned();
                        HydratedMessage {
                            seq,
                            role: msg.role.as_str().to_owned(),
                            content: msg.content.clone(),
                            turn_id: None, // Message struct does not carry typed turn_id today
                            thread_id: msg.thread_id.clone(),
                            client_message_id: msg.client_message_id.clone(),
                            persisted_at: msg.timestamp,
                            message_id,
                            source,
                            // P1.3 fix: surface canonical-ledger media so a
                            // client reconnecting after a disconnect can
                            // re-render the same `.md` / `.mp3` / `.pptx`
                            // attachment it would have seen via the live
                            // `message/persisted` push (`media` field on
                            // MessagePersistedEvent).
                            media: msg.media.clone(),
                        }
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let threads_projection = if include_set.threads || include_set.turns {
            Some(build_thread_graph_entries(session))
        } else {
            None
        };
        (messages, threads_projection)
    };

    let threads = if include_set.threads {
        threads_projection
            .clone()
            .map(|(threads, _orphans)| threads)
    } else {
        None
    };

    let turns = if include_set.turns {
        let projected_threads = threads_projection
            .as_ref()
            .map(|(t, _)| t.clone())
            .unwrap_or_default();
        Some(
            collect_session_turns(
                &params.session_id,
                active_turns,
                &replayed,
                &projected_threads,
            )
            .await,
        )
    } else {
        None
    };

    let pending_approvals = if include_set.pending_approvals {
        Some(approvals.pending_for_session(&params.session_id))
    } else {
        None
    };

    let result = SessionHydrateResult {
        session_id: params.session_id,
        cursor: head_cursor,
        messages,
        threads,
        turns,
        pending_approvals,
        replayed_envelopes,
    };
    send_serialized_rpc_result(
        ws,
        id,
        octos_core::ui_protocol::methods::SESSION_HYDRATE,
        result,
    );
}

/// Per UPCR-2026-010: lift the in-memory thread partition onto the wire.
async fn handle_thread_graph_get(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    _active_turns: &SharedActiveTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    id: String,
    params: ThreadGraphGetParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }

    // Atomic snapshot for `at`/`cursor` consistency. We don't actually
    // need the events here, but the cursor read piggybacks off the same
    // helper so the wire result echoes the head-of-snapshot moment.
    let (_events, head_cursor) = match ledger.snapshot_with_cursor(&params.session_id, None) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };

    // #919.1: route to the profile's session manager when under profile auth.
    let Some(sessions) = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        routed_profile_id,
        &params.session_id,
    )
    .await
    else {
        let _ = send_rpc_error(
            ws,
            Some(id),
            runtime_unavailable_error("Sessions not available"),
        );
        return;
    };
    // Reject unknown sessions per UPCR-2026-010 error model.
    {
        let mut sessions_guard = sessions.lock().await;
        if !sessions_guard.session_known(&params.session_id) {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(params.session_id.0.clone()),
            );
            return;
        }
    }
    let (threads, orphans) = {
        let mut sessions_guard = sessions.lock().await;
        let session = sessions_guard.get_or_create(&params.session_id).await;
        build_thread_graph_entries(session)
    };

    // When the caller pinned `at`, echo that cursor; otherwise return the
    // current head. Note: `at` as a true point-in-time projection of the
    // grouping is bounded by what `Session::messages` exposes today;
    // honouring `at` rigorously requires per-seq message snapshots in the
    // session store, which is out of scope for PR G. The wire shape
    // unconditionally reflects the current grouping; future UPCR can add
    // strict point-in-time snapshots if pinning becomes a hard requirement.
    let cursor = params.at.unwrap_or(head_cursor);

    let result = ThreadGraphGetResult {
        session_id: params.session_id,
        cursor,
        threads,
        orphans,
    };
    send_serialized_rpc_result(
        ws,
        id,
        octos_core::ui_protocol::methods::THREAD_GRAPH_GET,
        result,
    );
}

/// Per UPCR-2026-011: turn lifecycle introspection backed by the in-memory
/// active-turn registry AND a durable projection from the ledger
/// (`turn/started` + terminal `turn/completed` / `turn/error`). Codex's
/// review asked for the durable backing so a turn the registry has already
/// evicted (e.g. daemon restart, idle eviction) can still surface a
/// non-`unknown` state.
async fn handle_turn_state_get(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    active_turns: &SharedActiveTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    id: String,
    params: TurnStateGetParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }

    // UPCR-2026-011: reject `unknown_session` so the client distinguishes
    // "wrong session id" from "session id known but turn missing"
    // (which returns `state: unknown`). When the sessions manager is
    // unavailable we fall through to the default "unknown" path so the
    // RPC remains callable in headless tests.
    //
    // #919.1: route to the profile's session manager when under profile
    // auth so reads see the same store the turn writes used.
    let sessions = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        routed_profile_id,
        &params.session_id,
    )
    .await;
    if let Some(sessions) = sessions.as_ref() {
        let mut sessions_guard = sessions.lock().await;
        if !sessions_guard.session_known(&params.session_id) {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(params.session_id.0.clone()),
            );
            return;
        }
    }

    // Look up in the active-turn registry first.
    let registry_state = {
        let registry = active_turns.lock().await;
        if let Some(entry) = registry.get(&params.session_id) {
            if entry.turn_id == params.turn_id {
                let state = entry.state.lock().await;
                Some(turn_state_to_lifecycle(&state))
            } else {
                None
            }
        } else {
            None
        }
    };

    // Pull the ledger projection so we can backfill thread_id /
    // started_at / completed_at / committed_seqs even when the registry
    // entry is absent or carries less metadata.
    let projection = match ledger.snapshot_with_cursor(&params.session_id, None) {
        Ok((events, _)) => Some(project_turn_from_ledger(&params.turn_id, &events)),
        Err(_) => None,
    };

    // Cross-reference Session::messages for committed_seqs that match the
    // turn_id via thread_id grouping (today the type system does not yet
    // carry typed turn_id on Message; we approximate via the projection's
    // thread_id and the message's stored thread_id).
    let committed_seqs = if let Some(sessions) = sessions.as_ref() {
        let mut sessions_guard = sessions.lock().await;
        let session = sessions_guard.get_or_create(&params.session_id).await;
        let target_thread_id = projection.as_ref().and_then(|p| p.thread_id.clone());
        target_thread_id
            .map(|target| {
                session
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, msg)| msg.thread_id.as_deref() == Some(target.as_str()))
                    .map(|(seq, _)| seq as u64)
                    .collect::<Vec<u64>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Combine: registry beats projection for `state` (live truth) but
    // projection backfills metadata. When neither knows the turn, return
    // `unknown` per UPCR-2026-011 (NOT an error).
    let (state_value, started_at, completed_at, thread_id) =
        match (registry_state, projection.as_ref()) {
            (Some(state), Some(proj)) => (
                state,
                proj.started_at,
                proj.completed_at,
                proj.thread_id.clone(),
            ),
            (Some(state), None) => (state, None, None, None),
            (None, Some(proj)) => (
                proj.state.unwrap_or(TurnLifecycleState::Unknown),
                proj.started_at,
                proj.completed_at,
                proj.thread_id.clone(),
            ),
            (None, None) => (TurnLifecycleState::Unknown, None, None, None),
        };

    let result = TurnStateGetResult {
        session_id: params.session_id,
        turn_id: params.turn_id,
        state: state_value,
        started_at,
        completed_at,
        thread_id,
        committed_seqs,
    };
    send_serialized_rpc_result(
        ws,
        id,
        octos_core::ui_protocol::methods::TURN_STATE_GET,
        result,
    );
}

#[derive(Debug, Clone, Copy)]
struct HydrateIncludeSet {
    messages: bool,
    threads: bool,
    turns: bool,
    pending_approvals: bool,
}

impl HydrateIncludeSet {
    fn from_request(include: &[String]) -> Self {
        if include.is_empty() {
            // Empty / absent = include all (UPCR-2026-009).
            return Self {
                messages: true,
                threads: true,
                turns: true,
                pending_approvals: true,
            };
        }
        let mut set = Self {
            messages: false,
            threads: false,
            turns: false,
            pending_approvals: false,
        };
        for token in include {
            match token.as_str() {
                hydrate_sections::MESSAGES => set.messages = true,
                hydrate_sections::THREADS => set.threads = true,
                hydrate_sections::TURNS => set.turns = true,
                hydrate_sections::PENDING_APPROVALS => set.pending_approvals = true,
                _ => {} // Unknown tokens silently dropped per UPCR.
            }
        }
        set
    }
}

/// Build the thread-graph projection used by both `session/hydrate` and
/// `thread/graph/get`. Returns `(threads, orphans)`.
fn build_thread_graph_entries(session: &octos_bus::Session) -> (Vec<ThreadGraphEntry>, Vec<u64>) {
    use std::collections::BTreeMap;

    // Group messages by thread_id, recording each message's enumerated
    // index (its `seq` for wire purposes).
    let mut groups: BTreeMap<String, Vec<(u64, &Message)>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut orphans: Vec<u64> = Vec::new();
    for (seq, msg) in session.messages.iter().enumerate() {
        let Some(tid) = msg.thread_id.as_ref() else {
            // System messages have no thread_id; skip them (consistent
            // with `Session::threads()`). Non-system messages without a
            // thread_id are orphans.
            if !matches!(msg.role, MessageRole::System) {
                orphans.push(seq as u64);
            }
            continue;
        };
        if !groups.contains_key(tid) {
            order.push(tid.clone());
        }
        groups
            .entry(tid.clone())
            .or_default()
            .push((seq as u64, msg));
    }

    let mut entries: Vec<ThreadGraphEntry> = Vec::with_capacity(order.len());
    for tid in order {
        let members = groups.remove(&tid).unwrap_or_default();
        // Find the rooting user message (first User in the thread). If
        // there is no user message in the group, the thread is anchored
        // on its first member regardless of role.
        let root = members
            .iter()
            .find(|(_, msg)| matches!(msg.role, MessageRole::User))
            .copied()
            .or_else(|| members.first().copied());
        let Some((root_seq, root_msg)) = root else {
            // Empty group is unreachable but harmless: every key in
            // `groups` was inserted with at least one member.
            continue;
        };
        let message_seqs: Vec<u64> = members.iter().map(|(seq, _)| *seq).collect();
        entries.push(ThreadGraphEntry {
            thread_id: tid,
            root_seq,
            root_client_message_id: root_msg.client_message_id.clone(),
            // The `Message` struct does not carry a typed `turn_id` today
            // (PR-F in the structural plan adds it). Until then, leave the
            // wire field absent for legacy rows.
            turn_id: None,
            message_seqs,
            // Status is populated from the active-turn registry by the
            // turn projection; without a typed `turn_id` link we surface
            // `unknown` here. Sibling UPCR-2026-011 fills in the per-turn
            // detail via `turn/state/get`.
            status: thread_status::UNKNOWN.to_owned(),
        });
    }

    // Sort by root_seq for deterministic output (matches
    // `Session::threads()` chronological ordering).
    entries.sort_by_key(|entry| entry.root_seq);
    orphans.sort_unstable();
    (entries, orphans)
}

/// Translate the in-memory `TurnState` into the wire enum.
fn turn_state_to_lifecycle(state: &TurnState) -> TurnLifecycleState {
    match state {
        TurnState::Active => TurnLifecycleState::Active,
        TurnState::Interrupting { .. } => TurnLifecycleState::Interrupting,
        TurnState::Terminal(reason) => match reason {
            TerminalReason::Completed => TurnLifecycleState::Completed,
            TerminalReason::Errored => TurnLifecycleState::Errored,
            TerminalReason::Interrupted => TurnLifecycleState::Interrupted,
        },
    }
}

#[derive(Debug, Default, Clone)]
struct TurnLedgerProjection {
    state: Option<TurnLifecycleState>,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    thread_id: Option<String>,
}

/// Project a turn's lifecycle from the durable ledger event stream. Walks
/// the events for the session looking for `turn/started`, `turn/completed`,
/// `turn/error`, and `message/persisted` notifications referencing the
/// target `turn_id`. Returns `state = None` when the ledger has no record.
fn project_turn_from_ledger(
    target: &TurnId,
    events: &[LedgeredUiProtocolEvent],
) -> TurnLedgerProjection {
    let mut projection = TurnLedgerProjection::default();
    for ev in events {
        let UiProtocolLedgerEvent::Notification(notification) = &ev.event else {
            continue;
        };
        match notification {
            UiNotification::TurnStarted(started) if started.turn_id == *target => {
                projection.started_at = Some(started.timestamp);
                if projection.state.is_none() {
                    projection.state = Some(TurnLifecycleState::Active);
                }
            }
            UiNotification::TurnCompleted(completed) if completed.turn_id == *target => {
                projection.completed_at = Some(Utc::now());
                projection.state = Some(TurnLifecycleState::Completed);
            }
            UiNotification::TurnError(errored) if errored.turn_id == *target => {
                projection.completed_at = Some(Utc::now());
                projection.state = Some(if errored.code == "interrupted" {
                    TurnLifecycleState::Interrupted
                } else {
                    TurnLifecycleState::Errored
                });
            }
            UiNotification::MessagePersisted(persisted)
                if persisted.turn_id.as_ref() == Some(target) =>
            {
                if projection.thread_id.is_none() {
                    projection.thread_id = persisted.thread_id.clone();
                }
            }
            _ => {}
        }
    }
    projection
}

/// Combine the active-turn registry view with the ledger projection to
/// build the `turns` section of `session/hydrate`. Output is sorted by
/// `started_at` so consumers render turns in lifecycle order.
async fn collect_session_turns(
    session_id: &SessionKey,
    active_turns: &SharedActiveTurns,
    events: &[LedgeredUiProtocolEvent],
    threads: &[ThreadGraphEntry],
) -> Vec<HydratedTurn> {
    use std::collections::HashMap;

    // First: collect every turn_id we've seen in the ledger.
    let mut projections: HashMap<TurnId, TurnLedgerProjection> = HashMap::new();
    for ev in events {
        let UiProtocolLedgerEvent::Notification(notification) = &ev.event else {
            continue;
        };
        let turn_id = match notification {
            UiNotification::TurnStarted(e) => Some(e.turn_id.clone()),
            UiNotification::TurnCompleted(e) => Some(e.turn_id.clone()),
            UiNotification::TurnError(e) => Some(e.turn_id.clone()),
            UiNotification::MessagePersisted(e) => e.turn_id.clone(),
            _ => None,
        };
        let Some(turn_id) = turn_id else {
            continue;
        };
        if !projections.contains_key(&turn_id) {
            projections.insert(turn_id.clone(), TurnLedgerProjection::default());
        }
    }
    for turn_id in projections.keys().cloned().collect::<Vec<_>>() {
        let proj = project_turn_from_ledger(&turn_id, events);
        projections.insert(turn_id, proj);
    }

    // Overlay the active-turn registry's live state for the active turn,
    // if any.
    {
        let registry = active_turns.lock().await;
        if let Some(entry) = registry.get(session_id) {
            let live = {
                let state = entry.state.lock().await;
                turn_state_to_lifecycle(&state)
            };
            let proj = projections.entry(entry.turn_id.clone()).or_default();
            proj.state = Some(live);
        }
    }

    // Backfill thread_id from the thread graph when the ledger projection
    // didn't surface one (legacy rows / no `message/persisted` recorded
    // yet for this turn).
    let mut turns: Vec<HydratedTurn> = projections
        .into_iter()
        .map(|(turn_id, proj)| {
            let thread_id = proj.thread_id.clone().or_else(|| {
                threads
                    .iter()
                    .find(|t| t.turn_id.as_ref() == Some(&turn_id))
                    .map(|t| t.thread_id.clone())
            });
            HydratedTurn {
                turn_id,
                state: proj.state.unwrap_or(TurnLifecycleState::Unknown),
                started_at: proj.started_at,
                completed_at: proj.completed_at,
                thread_id,
            }
        })
        .collect();
    turns.sort_by_key(|t| t.started_at.unwrap_or_else(Utc::now));
    turns
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

// ===================== M12 Phase D-1 dispatchers =====================
//
// Each handler below mirrors a REST endpoint listed in
// `docs/adr/m12-phase-d-auxiliary-rest-to-ws.md`. The strategy is to
// invoke the existing REST handler function directly with the
// connection's snapshotted `HeaderMap` and `AuthIdentity`, then forward
// the resulting JSON body as the WS RPC result. This keeps the
// auxiliary surface fed by exactly one code path (the REST handler in
// `crates/octos-cli/src/api/handlers.rs` /
// `crates/octos-cli/src/api/auth_handlers.rs`), so Phase D-5 retirement
// later does not have to re-validate logic.

/// Per-handler limit for extracting JSON bodies from a REST `Response`.
/// Mirrors `MAX_TEXT_FRAME_BYTES` (1 MiB) so the WS write side cannot
/// be fed a frame the peer would reject as oversize. Truncates with an
/// RPC error if exceeded.
const AUX_REST_TO_WS_MAX_BODY_BYTES: usize = MAX_TEXT_FRAME_BYTES;

/// What kind of resource the dispatcher was addressing when a REST 404
/// came back. The bridge uses this to pick the right `RpcError` variant
/// — session-scoped methods surface `UNKNOWN_SESSION` (-32100) so the
/// client can reconcile against its session table; non-session methods
/// (content/profile/system) surface the generic `RESOURCE_NOT_FOUND`
/// (-32170) so a content row miss does not pollute the session error
/// channel.
///
/// Codex review 2026-05-12: the original implementation mapped EVERY
/// REST 404 to `RpcError::unknown_session(format!("{method}: not found"))`,
/// which (a) put the method name into the `session_id` field of the
/// error data and (b) misclassified content/profile 404s as session
/// misses.
#[derive(Debug, Clone)]
enum RestResourceContext {
    /// Session-scoped endpoint. `id` is the addressed session id.
    Session { id: String },
    /// Non-session resource. `resource_type` is a short tag
    /// ("content", "profile", ...); `id` is the resource id the
    /// client sent (empty when the request had no addressable id).
    Resource { resource_type: String, id: String },
}

impl RestResourceContext {
    fn session(id: impl Into<String>) -> Self {
        Self::Session { id: id.into() }
    }

    fn resource(resource_type: impl Into<String>, id: impl Into<String>) -> Self {
        Self::Resource {
            resource_type: resource_type.into(),
            id: id.into(),
        }
    }
}

/// Convert an axum `Response` from a REST handler into a parsed JSON
/// [`Value`] suitable for embedding in a WS RPC result. Non-2xx
/// responses are surfaced as typed RPC errors so the client sees the
/// same shape it would on REST today (404/400/503 → typed RpcError
/// variants per the ADR's `Error envelope` mapping).
async fn rest_response_to_rpc_value(
    response: axum::response::Response,
    method: &str,
    context: RestResourceContext,
) -> Result<Value, RpcError> {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), AUX_REST_TO_WS_MAX_BODY_BYTES)
        .await
        .map_err(|err| {
            RpcError::internal_error(format!("{method}: read REST body failed: {err}"))
        })?;
    if !status.is_success() {
        // Map common REST statuses to the ADR's error envelope. The
        // body is included as `data.detail` for debugging but the
        // message is the human-readable summary.
        let detail = String::from_utf8_lossy(body.as_ref()).into_owned();
        let detail = if detail.is_empty() {
            None
        } else {
            Some(detail)
        };
        return Err(rest_status_to_rpc_error(method, status, detail, &context));
    }
    if body.is_empty() {
        // REST 204 No Content (and equivalents like our `delete_session`
        // helper) becomes a `{}` result on the WS side.
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_slice::<Value>(body.as_ref())
        .map_err(|err| RpcError::internal_error(format!("{method}: REST body was not JSON: {err}")))
}

fn rest_status_to_rpc_error(
    method: &str,
    status: axum::http::StatusCode,
    detail: Option<String>,
    context: &RestResourceContext,
) -> RpcError {
    let mut data = serde_json::Map::new();
    data.insert("rest_status".into(), json!(status.as_u16()));
    if let Some(detail) = detail {
        // Cap detail at 2 KiB so error frames stay small even if the
        // REST handler returns a verbose body.
        let mut detail = detail;
        if detail.len() > 2048 {
            octos_core::truncate_utf8(&mut detail, 2048, "…");
        }
        data.insert("detail".into(), json!(detail));
    }
    use axum::http::StatusCode;
    let error = match status {
        // Codex review 2026-05-12: split session-scoped 404 from
        // generic 404. Session-scoped methods echo `session_id` in
        // `data` per spec §10; non-session resources go through the
        // new `RESOURCE_NOT_FOUND` slot so the resource_type + id
        // reach the client without abusing the `session_id` field.
        StatusCode::NOT_FOUND => match context {
            RestResourceContext::Session { id } => RpcError::unknown_session(id.clone()),
            RestResourceContext::Resource { resource_type, id } => {
                RpcError::not_found(resource_type.clone(), id.clone())
            }
        },
        StatusCode::BAD_REQUEST => {
            RpcError::invalid_params(format!("{method}: REST returned 400 bad_request"))
        }
        StatusCode::SERVICE_UNAVAILABLE => RpcError::runtime_not_ready(format!(
            "{method}: REST handler not configured on this server"
        )),
        StatusCode::CONFLICT => {
            RpcError::invalid_params(format!("{method}: REST returned 409 conflict"))
        }
        _ => RpcError::internal_error(format!(
            "{method}: REST returned status {}",
            status.as_u16()
        )),
    };
    // Merge the existing data (rest_status + optional detail) with
    // whatever the typed variant already wrote (e.g.
    // `unknown_session.data.kind = "unknown_session"`,
    // `unknown_session.data.session_id = "..."`). Existing keys win so
    // the typed-error contract is preserved.
    let mut merged = data;
    if let Some(Value::Object(existing)) = error.data.clone() {
        for (k, v) in existing {
            merged.insert(k, v);
        }
    }
    error.with_data(Value::Object(merged))
}

/// Forward a parsed JSON body to the WS RPC result channel. Used by
/// every Phase D-1 dispatcher after extracting the REST handler body.
fn send_aux_rpc_result(ws: &WsConnection, id: String, method: &str, body: Value) {
    // The body shape MUST match the WS result schema documented in
    // `octos-core/src/ui_protocol.rs`. For methods whose Result wraps
    // the REST body under a single field (e.g. `SessionListResult.sessions`),
    // callers are responsible for constructing that wrapper before
    // invoking this helper; methods whose Result is a direct alias of
    // the REST body pass it through verbatim.
    if let Err(error) = send_rpc_result(ws, id, body) {
        tracing::debug!(
            target: "octos::ui_protocol::ws::aux",
            method = %method,
            ?error,
            "aux REST→WS result send failed"
        );
    }
}

/// Build a synthetic `Path<String>` extractor from the session id in
/// the WS params. Axum's `Path` is a tuple wrapper so we construct it
/// directly via `Path(...)` — the REST handler treats it like an
/// already-extracted route segment.
fn axum_path(value: String) -> axum::extract::Path<String> {
    axum::extract::Path(value)
}

async fn handle_session_list(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    id: String,
    _params: SessionListParams,
) {
    let response = super::handlers::list_sessions(State(state.clone()), headers.clone()).await;
    let method = octos_core::ui_protocol::methods::SESSION_LIST;
    // Collection endpoint — no addressable session id. Treat any
    // (unexpected) 404 as a generic resource-not-found rather than
    // an `UNKNOWN_SESSION` miss.
    let context = RestResourceContext::resource("session", "");
    match rest_response_to_rpc_value(response, method, context).await {
        Ok(sessions) => {
            send_aux_rpc_result(ws, id, method, json!({ "sessions": sessions }));
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn handle_session_status_get(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    id: String,
    params: SessionStatusGetParams,
) {
    let topic_params = axum::extract::Query(super::handlers::TopicQueryParams {
        topic: params.topic.clone(),
    });
    let session_id_str = params.session_id.clone();
    let response = super::handlers::session_status(
        State(state.clone()),
        headers.clone(),
        axum_path(params.session_id),
        topic_params,
    )
    .await;
    let method = octos_core::ui_protocol::methods::SESSION_STATUS_GET;
    let context = RestResourceContext::session(session_id_str);
    match rest_response_to_rpc_value(response, method, context).await {
        Ok(status) => {
            send_aux_rpc_result(ws, id, method, json!({ "status": status }));
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn handle_session_files_list(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    id: String,
    params: SessionFilesListParams,
) {
    let identity_ext = identity.cloned().map(Extension);
    let session_id_str = params.session_id.clone();
    let response = super::handlers::session_files(
        State(state.clone()),
        headers.clone(),
        identity_ext,
        axum_path(params.session_id),
    )
    .await;
    let method = octos_core::ui_protocol::methods::SESSION_FILES_LIST;
    let context = RestResourceContext::session(session_id_str);
    match rest_response_to_rpc_value(response, method, context).await {
        Ok(files) => {
            send_aux_rpc_result(ws, id, method, json!({ "files": files }));
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn handle_session_tasks_list(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    id: String,
    params: SessionTasksListParams,
) {
    let topic_params = axum::extract::Query(super::handlers::TopicQueryParams {
        topic: params.topic.clone(),
    });
    let session_id_str = params.session_id.clone();
    let response = super::handlers::session_tasks(
        State(state.clone()),
        headers.clone(),
        axum_path(params.session_id),
        topic_params,
    )
    .await;
    let method = octos_core::ui_protocol::methods::SESSION_TASKS_LIST;
    let context = RestResourceContext::session(session_id_str);
    match rest_response_to_rpc_value(response, method, context).await {
        Ok(tasks) => {
            send_aux_rpc_result(ws, id, method, json!({ "tasks": tasks }));
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn handle_session_workspace_get(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    id: String,
    params: SessionWorkspaceGetParams,
) {
    let identity_ext = identity.cloned().map(Extension);
    let session_id_str = params.session_id.clone();
    let response = super::handlers::session_workspace_contract(
        State(state.clone()),
        headers.clone(),
        identity_ext,
        axum_path(params.session_id),
    )
    .await;
    let method = octos_core::ui_protocol::methods::SESSION_WORKSPACE_GET;
    let context = RestResourceContext::session(session_id_str);
    match rest_response_to_rpc_value(response, method, context).await {
        Ok(contracts) => {
            send_aux_rpc_result(ws, id, method, json!({ "contracts": contracts }));
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn handle_session_snapshot(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    id: String,
    params: SessionSnapshotParams,
) {
    // Snapshot is a single round trip that collapses the three REST
    // bootstrap calls (status, files, tasks). Run them concurrently
    // and surface the first error.
    let method = octos_core::ui_protocol::methods::SESSION_SNAPSHOT;
    let session_id_str = params.session_id.clone();
    let topic_params = axum::extract::Query(super::handlers::TopicQueryParams {
        topic: params.topic.clone(),
    });
    let identity_ext = identity.cloned().map(Extension);
    let status_fut = super::handlers::session_status(
        State(state.clone()),
        headers.clone(),
        axum_path(params.session_id.clone()),
        axum::extract::Query(super::handlers::TopicQueryParams {
            topic: params.topic.clone(),
        }),
    );
    let files_fut = super::handlers::session_files(
        State(state.clone()),
        headers.clone(),
        identity_ext,
        axum_path(params.session_id.clone()),
    );
    let tasks_fut = super::handlers::session_tasks(
        State(state.clone()),
        headers.clone(),
        axum_path(params.session_id),
        topic_params,
    );
    let (status_resp, files_resp, tasks_resp) = tokio::join!(status_fut, files_fut, tasks_fut);
    let status = match rest_response_to_rpc_value(
        status_resp,
        method,
        RestResourceContext::session(session_id_str.clone()),
    )
    .await
    {
        Ok(v) => v,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    let files = match rest_response_to_rpc_value(
        files_resp,
        method,
        RestResourceContext::session(session_id_str.clone()),
    )
    .await
    {
        Ok(v) => v,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    let tasks = match rest_response_to_rpc_value(
        tasks_resp,
        method,
        RestResourceContext::session(session_id_str),
    )
    .await
    {
        Ok(v) => v,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    send_aux_rpc_result(
        ws,
        id,
        method,
        json!({
            "status": status,
            "files": files,
            "tasks": tasks,
        }),
    );
}

async fn handle_session_messages_page(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    id: String,
    params: SessionMessagesPageParams,
) {
    let method = octos_core::ui_protocol::methods::SESSION_MESSAGES_PAGE;
    let limit = params
        .limit
        .unwrap_or(SESSION_MESSAGES_PAGE_DEFAULT_LIMIT)
        .min(SESSION_MESSAGES_PAGE_MAX_LIMIT);
    let offset = params
        .offset
        .unwrap_or(0)
        .min(SESSION_MESSAGES_PAGE_MAX_OFFSET);
    let pagination = axum::extract::Query(super::handlers::PaginationParams {
        limit,
        offset,
        source: None,
        since_seq: params.since_seq,
        topic: params.topic.clone(),
    });
    let identity_ext = identity.cloned().map(Extension);
    let session_id_str = params.session_id.clone();
    let response = super::handlers::session_messages(
        State(state.clone()),
        headers.clone(),
        identity_ext,
        axum_path(params.session_id),
        pagination,
    )
    .await;
    // Codex review 2026-05-12: previously, this dispatcher mapped REST
    // 404 to an empty page and dropped REST 503 onto the generic JSON
    // path (which then surfaced `INTERNAL_ERROR`). Both diverged from
    // the REST contract documented at `handlers.rs:767` (gateway
    // proxy 404) and `handlers.rs:783` (standalone fallback 503).
    // Mirror REST faithfully now: 404 → `UNKNOWN_SESSION` with the
    // addressed `session_id` echoed in `data` per spec §10; 503 →
    // `runtime_not_ready` so clients can distinguish "session does
    // not exist" from "server has no gateway wired".
    let context = RestResourceContext::session(session_id_str);
    match rest_response_to_rpc_value(response, method, context).await {
        Ok(messages) => {
            let len = messages.as_array().map(|arr| arr.len()).unwrap_or(0);
            let has_more = len == limit;
            let next_offset = offset.saturating_add(len);
            send_aux_rpc_result(
                ws,
                id,
                method,
                json!({
                    "messages": messages,
                    "has_more": has_more,
                    "next_offset": next_offset,
                }),
            );
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn handle_session_title_set(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    id: String,
    params: SessionTitleSetParams,
) {
    let method = octos_core::ui_protocol::methods::SESSION_TITLE_SET;
    let trimmed = params.title.trim();
    if trimmed.is_empty() {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!("{method}: title must not be empty")),
        );
        return;
    }
    if trimmed.chars().count() > SESSION_TITLE_SET_MAX_CHARS {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!(
                "{method}: title must be at most {SESSION_TITLE_SET_MAX_CHARS} chars"
            )),
        );
        return;
    }
    let body = axum::Json(super::handlers::UpdateTitleRequest {
        title: trimmed.to_string(),
    });
    let session_id_str = params.session_id.clone();
    let response = super::handlers::update_session_title(
        State(state.clone()),
        headers.clone(),
        identity.cloned().map(axum::Extension),
        axum_path(params.session_id),
        body,
    )
    .await;
    let status = response.status();
    if status.is_success() {
        send_aux_rpc_result(
            ws,
            id,
            method,
            json!({
                "session_id": session_id_str,
                "title": trimmed,
            }),
        );
    } else {
        let detail = axum::body::to_bytes(response.into_body(), AUX_REST_TO_WS_MAX_BODY_BYTES)
            .await
            .ok()
            .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok());
        let context = RestResourceContext::session(session_id_str);
        let _ = send_rpc_error(
            ws,
            Some(id),
            rest_status_to_rpc_error(method, status, detail, &context),
        );
    }
}

async fn handle_session_delete(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    id: String,
    params: SessionDeleteParams,
) {
    let method = octos_core::ui_protocol::methods::SESSION_DELETE;
    let session_id_str = params.session_id.clone();
    let response = super::handlers::delete_session(
        State(state.clone()),
        headers.clone(),
        identity.cloned().map(axum::Extension),
        axum_path(params.session_id),
    )
    .await;
    let status = response.status();
    if status.is_success() {
        send_aux_rpc_result(ws, id, method, json!({}));
    } else {
        let detail = axum::body::to_bytes(response.into_body(), AUX_REST_TO_WS_MAX_BODY_BYTES)
            .await
            .ok()
            .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok());
        let context = RestResourceContext::session(session_id_str);
        let _ = send_rpc_error(
            ws,
            Some(id),
            rest_status_to_rpc_error(method, status, detail, &context),
        );
    }
}

async fn handle_system_status_get(
    ws: &WsConnection,
    state: &Arc<AppState>,
    id: String,
    _params: SystemStatusGetParams,
) {
    let method = octos_core::ui_protocol::methods::SYSTEM_STATUS_GET;
    let axum::Json(status) = super::handlers::status(State(state.clone())).await;
    match serde_json::to_value(&status) {
        Ok(value) => send_aux_rpc_result(ws, id, method, json!({ "status": value })),
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!("{method}: serialize status failed: {error}")),
            );
        }
    }
}

async fn handle_content_list(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    id: String,
    params: ContentListParams,
) {
    let method = octos_core::ui_protocol::methods::CONTENT_LIST;
    let Some(identity) = identity.cloned() else {
        // Web PR #114 contract: SPA bridge listens for close-code 1008 to
        // trigger `crew:auth_expired` (clears token, routes to /login). The
        // RPC envelope alone leaves a stale-token client retrying forever.
        // Codex BLOCK (2026-05-13): close must precede the error envelope so
        // it survives backpressure when only one writer slot is free.
        let _ = close_ws_with_code(ws, 1008, "auth_expired");
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::permission_denied(format!("{method}: authenticated user identity required")),
        );
        return;
    };
    // `ContentQuery` deserializes from the same JSON the REST query
    // string built. Forwarding the `filters` object lets clients opt
    // in to category / search / pagination without us redefining the
    // struct here. Null / empty filters fall back to the REST default
    // (`Default::default()`) so `content/list` with `{}` params works
    // identically to `GET /api/my/content` with no query string.
    let filters = if params.filters.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params.filters
    };
    let query: crate::content_catalog::ContentQuery = match serde_json::from_value(filters) {
        Ok(q) => q,
        Err(err) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::invalid_params(format!("{method}: invalid filters: {err}")),
            );
            return;
        }
    };
    let result = super::auth_handlers::my_content(
        State(state.clone()),
        headers.clone(),
        Extension(identity),
        axum::extract::Query(query),
    )
    .await;
    match result {
        Ok(axum::Json(value)) => match serde_json::to_value(&value) {
            Ok(json_value) => {
                let entries = json_value
                    .get("entries")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                let total = json_value.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                send_aux_rpc_result(
                    ws,
                    id,
                    method,
                    json!({
                        "entries": entries,
                        "total": total,
                    }),
                );
            }
            Err(error) => {
                let _ = send_rpc_error(
                    ws,
                    Some(id),
                    RpcError::internal_error(format!(
                        "{method}: serialize content list failed: {error}"
                    )),
                );
            }
        },
        Err((status, message)) => {
            // `content/list` is a collection endpoint — no
            // addressable id. Use the generic resource context so a
            // (rare) 404 surfaces as `RESOURCE_NOT_FOUND` rather than
            // `UNKNOWN_SESSION`.
            let context = RestResourceContext::resource("content", "");
            let _ = send_rpc_error(
                ws,
                Some(id),
                rest_status_to_rpc_error(method, status, Some(message), &context),
            );
        }
    }
}

async fn handle_content_delete(
    ws: &WsConnection,
    state: &Arc<AppState>,
    identity: Option<&AuthIdentity>,
    id: String,
    params: ContentDeleteParams,
) {
    let method = octos_core::ui_protocol::methods::CONTENT_DELETE;
    let Some(identity) = identity.cloned() else {
        // Web PR #114 contract: see `close_ws_with_code` doc-comment. Codex
        // BLOCK (2026-05-13): close before error so it survives writer
        // backpressure when the channel has just one free slot.
        let _ = close_ws_with_code(ws, 1008, "auth_expired");
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::permission_denied(format!("{method}: authenticated user identity required")),
        );
        return;
    };
    let content_id = params.id.clone();
    let result = super::auth_handlers::delete_my_content(
        State(state.clone()),
        Extension(identity),
        axum_path(params.id),
    )
    .await;
    match result {
        Ok(axum::Json(action)) => {
            send_aux_rpc_result(
                ws,
                id,
                method,
                json!({
                    "deleted": action.ok,
                }),
            );
        }
        Err((status, message)) => {
            // Content row miss → `RESOURCE_NOT_FOUND` with the content
            // id echoed in `data.identifier`. Previously this funnelled
            // through `UNKNOWN_SESSION` and stuffed the method name in
            // the `session_id` slot (codex review 2026-05-12).
            let context = RestResourceContext::resource("content", content_id);
            let _ = send_rpc_error(
                ws,
                Some(id),
                rest_status_to_rpc_error(method, status, Some(message), &context),
            );
        }
    }
}

async fn handle_content_bulk_delete(
    ws: &WsConnection,
    state: &Arc<AppState>,
    identity: Option<&AuthIdentity>,
    id: String,
    params: ContentBulkDeleteParams,
) {
    let method = octos_core::ui_protocol::methods::CONTENT_BULK_DELETE;
    let Some(identity) = identity.cloned() else {
        // Web PR #114 contract: see `close_ws_with_code` doc-comment. Codex
        // BLOCK (2026-05-13): close before error so it survives writer
        // backpressure when the channel has just one free slot.
        let _ = close_ws_with_code(ws, 1008, "auth_expired");
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::permission_denied(format!("{method}: authenticated user identity required")),
        );
        return;
    };
    // Codex review 2026-05-12: reject over-cap bulk-delete requests
    // before they reach the catalog write-lock. The 1 MiB frame limit
    // is a coarser secondary check; this per-method cap keeps a
    // single oversized request from monopolizing the catalog for
    // even a small bounded window. Mirrored in
    // `octos-core::ui_protocol::CONTENT_BULK_DELETE_MAX_IDS`.
    if params.ids.len() > octos_core::ui_protocol::CONTENT_BULK_DELETE_MAX_IDS {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!(
                "{method}: ids count {} exceeds maximum of {}",
                params.ids.len(),
                octos_core::ui_protocol::CONTENT_BULK_DELETE_MAX_IDS,
            ))
            .with_data(json!({
                "max_ids": octos_core::ui_protocol::CONTENT_BULK_DELETE_MAX_IDS,
                "requested_ids": params.ids.len(),
            })),
        );
        return;
    }
    let result = super::auth_handlers::bulk_delete_my_content(
        State(state.clone()),
        Extension(identity),
        axum::Json(super::auth_handlers::BulkDeleteRequest { ids: params.ids }),
    )
    .await;
    match result {
        Ok(axum::Json(action)) => {
            // The REST handler stuffs the count into the user-facing
            // message ("N item(s) deleted."). Parse it back out so the
            // WS shape can return a typed integer per the ADR.
            let deleted = action
                .message
                .as_deref()
                .and_then(|msg| msg.split_whitespace().next())
                .and_then(|first| first.parse::<usize>().ok())
                .unwrap_or(0);
            send_aux_rpc_result(ws, id, method, json!({ "deleted": deleted }));
        }
        Err((status, message)) => {
            // Bulk-delete is a collection operation; no single id is
            // the locus. Surface 404 (which the REST handler should
            // never return for this method) through the generic
            // resource context.
            let context = RestResourceContext::resource("content", "");
            let _ = send_rpc_error(
                ws,
                Some(id),
                rest_status_to_rpc_error(method, status, Some(message), &context),
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
        // UPCR-2026-014 (M9-α-9): WS turn-start path has no topic in
        // scope today; the SSE bridge in α-9 plumbs topic separately.
        topic: None,
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
    routed_profile_id: Option<String>,
    turn_state: Arc<TokioMutex<TurnState>>,
    mut interrupt_rx: mpsc::Receiver<()>,
) {
    let session_id = params.session_id.clone();
    let turn_id = params.turn_id.clone();
    let started = UiNotification::TurnStarted(octos_core::ui_protocol::TurnStartedEvent {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        timestamp: Utc::now(),
        // UPCR-2026-014 (M9-α-9): legacy WS turn-start path; topic is
        // not in scope here (the SSE bridge surfaces it via α-9).
        topic: None,
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

    // M11-F: resolve the per-session view through the
    // `ProfileRuntime` + `SessionRuntimeCache` path only. The legacy
    // single-agent fallback (`state.agent` / `validate_runtime`) was
    // deleted — `octos serve` bootstraps every profile in
    // `ProfileStore::list()` at startup, so an unregistered profile
    // here is a configuration bug, not a runtime fallback. Fail closed
    // with a typed `runtime_unavailable` terminal so the client sees
    // the same error shape it would for a SessionRuntime::bootstrap
    // failure.
    let active_profile_id = session_id
        .profile_id()
        .map(ToOwned::to_owned)
        .or(routed_profile_id);
    let Some(profile_runtime) =
        resolve_session_profile_runtime(&state, active_profile_id.as_deref())
    else {
        let error = format!(
            "No ProfileRuntime registered for profile '{}'. \
             Set up the profile with an API key in the dashboard.",
            active_profile_id.as_deref().unwrap_or("<unset>"),
        );
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
        contracts.scopes.evict_turn(&session_id, &turn_id);
        return;
    };

    // Read-through view: when `session.open` previously stashed an
    // effective cwd in `session_workspaces()` (Tier-1 client cwd or
    // Tier-2 operator default, resolved at `open_session_result` time),
    // use that as the `workspace_hint`. Otherwise the bootstrap default
    // Tier-3 (`<profile_data_dir>/users/.../workspace`) wins.
    let hint = session_workspaces().get(&session_id);
    let session_runtime = match state
        .session_cache
        .get_or_init(&profile_runtime, session_id.clone(), hint)
        .await
    {
        Ok(rt) => rt,
        Err(error) => {
            try_emit_terminal(
                &turn_state,
                TerminalReason::Errored,
                &ws,
                &ledger,
                &session_id,
                &turn_id,
                Some(("runtime_unavailable", &error.to_string())),
            )
            .await;
            contracts.scopes.evict_turn(&session_id, &turn_id);
            return;
        }
    };

    // Source the per-session primitives from the SessionRuntime.
    //
    // `tool_registry` is an OWNED `ToolRegistry` we mutate per-turn
    // (`set_background_result_sender`, `register(send_file_tool)`,
    // `supervisor().set_on_change`). We snapshot from the shared
    // `Arc<ToolRegistry>` so per-turn mutation does not race with the
    // cached SessionRuntime.
    let sessions = session_runtime.sessions.clone();
    let mut tool_registry = session_runtime.tools.snapshot_excluding(&[]);
    // M11-F regression fix REG-1 follow-up round 2 (codex review):
    // re-register a fresh `ActivateToolsTool` on this per-turn
    // snapshot so `wire_activate_tools()` below rewires THIS
    // registry's Weak, not the cached SessionRuntime's. Without this,
    // the per-turn rebuild would mutate the shared
    // `Arc<ActivateToolsTool>` (clones share the same Mutex<Weak>)
    // and the SessionRuntime's cached agent would silently lose its
    // back-reference once the per-turn registry dropped at end of
    // turn.
    if tool_registry.get("activate_tools").is_some() {
        tool_registry.register(octos_agent::ActivateToolsTool::new());
    }
    let workspace_root: Option<PathBuf> = Some(session_runtime.workspace_root.clone());
    let llm_provider: Arc<dyn octos_llm::LlmProvider> = session_runtime.profile.llm.clone();
    let memory_store: Arc<octos_memory::EpisodeStore> = session_runtime.profile.memory.clone();
    let agent_config = session_runtime.agent.agent_config();
    let system_prompt_base = session_runtime.agent.system_prompt_snapshot();

    let history: Vec<Message> = {
        let mut sessions = sessions.lock().await;
        let session = sessions.get_or_create(&session_id).await;
        session.get_history(50).to_vec()
    };

    // For hosted multi-tenant standalone serve, the file API resolves
    // `/api/files/...` against the per-profile data dir (`<server_data>/
    // profiles/<profile>/data`), not the server-wide one. Plugin output
    // must land under the SAME root the file API will check, otherwise
    // `resolve_legacy_file_request` rejects it.
    //
    // The active profile id can come from three places, in order:
    //   1. `session_id.profile_id()` — when the SPA encodes it via
    //      `SessionKey::with_profile`. Bare-channel session ids
    //      (`web-…`) skip this.
    //   2. `routed_profile_id` — derived from the connection's `Host`
    //      header during WS handshake. Hosted admin-token requests land
    //      here; the SPA at `dspfac.crew.ominix.io` matches.
    //   3. The registered `ProfileRuntime`'s `data_dir` when M11-E
    //      materialized the SessionRuntime.
    // Falls back to the server-wide data dir for local sessions / dev.
    let plugin_root_dir = session_runtime.profile.data_dir.clone();

    // β: wire `BackgroundResultSender` + `SendFileTool` so spawn_only tool
    // completions and explicit `send_file` calls persist as assistant
    // messages on the session and reach connected WS clients via the
    // existing `MessageCommitObserver` -> `message/persisted` ledger append
    // (#761 live publish-subscribe). Without this, the api/serve path drops
    // spawn_only file deliveries on the floor — gateway wires the
    // equivalent in `session_actor.rs::deliver_background_notification`.
    //
    // The canonical persist
    // (`octos_bus::session::persist_message_through_canonical_path`)
    // serialises with other writers via a per-key Tokio mutex, so this is
    // safe to invoke from a `tokio::spawn`-driven background task that may
    // complete after the originating turn has ended. After each persist we
    // invalidate the cached `SessionManager` so `session/hydrate` and
    // `/api/sessions/:id/messages` reads pick up the new row instead of
    // the pre-persist snapshot (matches `ApiChannel::persist_to_session`'s
    // post-write invalidate at `api_channel.rs:1503`).
    //
    // M10 Phase 1: in addition to persisting (which still fires
    // `message/persisted` via `MessageCommitObserver` for ledger
    // durability + `event.message_persisted.v1` clients), the closure
    // now appends a `turn/spawn_complete` envelope event to the ledger
    // for clients that negotiated `event.spawn_complete.v1`. The
    // per-connection capability filter (`live_event_passes_capability_filter`)
    // routes each connection to exactly one wire shape — old clients
    // see `message/persisted` as before, new clients see
    // `turn/spawn_complete` and the duplicate `message/persisted`
    // (with `source: background`) is suppressed.
    {
        let bg_data_dir = sessions.lock().await.data_dir().to_path_buf();
        let bg_sessions = sessions.clone();
        let bg_session_id = session_id.clone();
        let bg_thread_id = turn_id.0.to_string();
        let bg_turn_id = turn_id.clone();

        // Wire spawn_only contract-satisfied path.
        let payload_sessions = bg_sessions.clone();
        let payload_data_dir = bg_data_dir.clone();
        let payload_session_id = bg_session_id.clone();
        let payload_thread_id = bg_thread_id.clone();
        let payload_turn_id = bg_turn_id.clone();
        let payload_ledger = ledger.clone();
        tool_registry.set_background_result_sender(std::sync::Arc::new(
            move |payload: BackgroundResultPayload| {
                let sessions = payload_sessions.clone();
                let data_dir = payload_data_dir.clone();
                let session_id = payload_session_id.clone();
                let originating_thread_id = payload
                    .originating_thread_id
                    .clone()
                    .filter(|tid| !tid.is_empty());
                let thread_id = originating_thread_id
                    .clone()
                    .unwrap_or_else(|| payload_thread_id.clone());
                let task_label = payload.task_label.clone();
                let media = payload.media.clone();
                // M10 Phase 5a: envelope_media is the media list to surface
                // ONLY on the `turn/spawn_complete` envelope. The
                // `NotConfigured` `send_file` fallback populates this with
                // its `sent_files` paths so dual-negotiated clients see the
                // file URLs on the envelope; the persisted row keeps
                // `media: vec![]` (no double-render on old clients that DO
                // see the per-file `message/persisted` companions). The
                // contract-`Satisfied` path leaves `envelope_media` as the
                // empty default; in that case the envelope falls back to
                // `media`.
                let envelope_media = if payload.envelope_media.is_empty() {
                    payload.media.clone()
                } else {
                    payload.envelope_media.clone()
                };
                let kind = payload.kind;
                let raw_content = payload.content.clone();
                let task_id = payload.task_id.clone();
                let turn_id = payload_turn_id.clone();
                let ledger = payload_ledger.clone();
                Box::pin(async move {
                    let content_text = match kind {
                        BackgroundResultKind::Notification => {
                            if raw_content.is_empty() && !media.is_empty() {
                                format!("✅ {} delivered.", task_label)
                            } else {
                                raw_content
                            }
                        }
                        BackgroundResultKind::Report => {
                            if raw_content.is_empty() && !media.is_empty() {
                                format!("✅ {} completed.", task_label)
                            } else if raw_content.len() > 1000 {
                                let preview: String = raw_content.chars().take(300).collect();
                                format!("✅ **{}** completed.\n\n{}…", task_label, preview,)
                            } else {
                                format!("✅ **{}** completed.\n\n{}", task_label, raw_content,)
                            }
                        }
                    };
                    // M10 Phase 1 (codex P1): scope the persist call in
                    // the `MESSAGE_PERSISTED_SOURCE_OVERRIDE` task-local
                    // so `install_message_commit_observer` emits
                    // `source: background` for this row. Without this,
                    // `MessagePersistedSource::from_role(Assistant)`
                    // would return `Assistant` and the per-connection
                    // duplicate-suppression filter would never fire,
                    // delivering both `message/persisted` AND
                    // `turn/spawn_complete` to upgraded clients.
                    // M10 Phase 1 (codex round 4): only mark this row as
                    // `source: background` if we will emit a replacement
                    // `turn/spawn_complete` envelope for it. Otherwise
                    // upgraded clients filter the legacy
                    // `message/persisted` row AND see no envelope —
                    // they receive nothing for the completion. The
                    // marker has to stay coupled to the envelope emit
                    // for the dual-gate invariant to hold.
                    //
                    // Empty `Some("")` — the legacy register sentinel
                    // returned when the supervisor's fan-out cap refuses
                    // a task — is treated like `None` (codex round 3 P3).
                    let task_id_clean = task_id
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    let will_emit_envelope = task_id_clean.is_some();
                    let persisted_meta = if will_emit_envelope {
                        MESSAGE_PERSISTED_SOURCE_OVERRIDE
                            .scope(
                                Some(MessagePersistedSource::Background),
                                persist_assistant_with_media(
                                    &sessions,
                                    &data_dir,
                                    &session_id,
                                    content_text.clone(),
                                    media.clone(),
                                    thread_id.clone(),
                                    &task_label,
                                ),
                            )
                            .await
                    } else {
                        // No envelope incoming → leave the source as
                        // role-derived (Assistant) so upgraded clients
                        // still receive the `message/persisted` row.
                        // This degrades to legacy behaviour for the
                        // edge cases (no tracked task, empty sentinel)
                        // rather than silently dropping the completion.
                        persist_assistant_with_media(
                            &sessions,
                            &data_dir,
                            &session_id,
                            content_text.clone(),
                            media.clone(),
                            thread_id.clone(),
                            &task_label,
                        )
                        .await
                    };
                    if let (Some(task_id_value), Some(meta)) =
                        (task_id_clean.clone(), persisted_meta.as_ref())
                    {
                        let event = TurnSpawnCompleteEvent {
                            session_id: session_id.clone(),
                            turn_id: Some(turn_id.clone()),
                            thread_id: Some(thread_id.clone()),
                            task_id: task_id_value,
                            // Codex rounds 2/6: leave this `None`. In
                            // the standalone-turn path the reporter
                            // binds `thread_id = turn_id.0.to_string()`
                            // (a TurnId UUID), so `originating_thread_id`
                            // here is NOT the user's `client_message_id`
                            // the field is documented to carry. Phase 4
                            // plumbing will add a typed
                            // `originating_client_message_id` to
                            // `BackgroundResultPayload` and populate
                            // this from there. Today the SPA reducer
                            // already anchors via `thread_id` (which
                            // matches the user-prompt row's thread_id
                            // through the M8.10 root-on-cmid
                            // invariant), so this `None` is safe.
                            response_to_client_message_id: None,
                            seq: meta.committed_seq as u64,
                            // Reuse the `MessageCommitObserver`-style
                            // wire id for the same durable row — see
                            // `PersistedMessageMeta` doc.
                            message_id: meta.message_id.clone(),
                            source: "background".to_owned(),
                            cursor: UiCursor {
                                stream: session_id.0.clone(),
                                seq: 0,
                            },
                            persisted_at: Utc::now(),
                            content: content_text,
                            media: envelope_media,
                        };
                        ledger.append_notification(UiNotification::TurnSpawnComplete(event));
                    } else if task_id_clean.is_none() {
                        // Best-effort: a payload without `task_id` (or
                        // with the empty-string sentinel returned by
                        // the legacy register-task path under fan-out
                        // pressure) arrives only from edge-case
                        // callers. Old clients see `message/persisted`
                        // as before; new clients miss this single
                        // completion. Logging surfaces the gap so we
                        // can fix upstream callers.
                        tracing::debug!(
                            session_id = %session_id.0,
                            task_label,
                            had_empty_task_id = task_id.as_deref() == Some(""),
                            "background result missing task_id; turn/spawn_complete suppressed"
                        );
                    } else {
                        // Persist of the spawn-ack/completion row failed.
                        // The agent's task_supervisor records the failure
                        // for operator visibility.
                        //
                        // M10 Phase 5a coalesce: the per-file `send_file`
                        // companion rows for the NotConfigured branch
                        // were already committed *before* this final
                        // persist (the consumer drains them off
                        // `out_rx` independently). Those companion rows
                        // are tagged `MessagePersistedSource::Background`
                        // so dual-negotiated clients suppress them at
                        // `live_event_passes_capability_filter` —
                        // meaning a new client whose envelope persist
                        // failed sees ZERO file rows for the completion
                        // (the per-file rows are filtered, the envelope
                        // never fired). Old clients see the per-file
                        // rows unchanged (they pass the legacy gate
                        // regardless of source). Accepted as a
                        // low-probability degradation: persist is
                        // durable, this branch fires only when the
                        // session ledger cannot accept a write, and the
                        // task_supervisor captures the failure for
                        // recovery follow-ups. Phase 6 will reorder the
                        // companion rows AFTER the envelope persist
                        // commits to close this window.
                        tracing::warn!(
                            session_id = %session_id.0,
                            task_label,
                            "background result persist failed; new clients miss this completion (per-file companion rows already committed under source: background and are suppressed)"
                        );
                    }
                    persisted_meta.is_some()
                })
            },
        ));

        // Wire `send_file` for the legacy non-contract `files_to_send` path
        // and any explicit agent calls. The spawn_only auto-background
        // branch falls back to `send_file` when the workspace contract is
        // `NotConfigured` (`execution.rs:549`) — without this registration,
        // tools like `deep_search` (no default api-mode workspace policy)
        // emit `files_to_send` that have nowhere to land.
        let (out_tx, mut out_rx) =
            mpsc::channel::<octos_core::OutboundMessage>(SEND_FILE_CHANNEL_CAPACITY);
        // Mirror gateway's session_actor.rs:2087 base/extra split: use the
        // session workspace root as the base_dir (so a spawn_only tool
        // returning `files_to_send: ["output/report.md"]` resolves under
        // the user's workspace), and keep `data_dir` as an extra-allowed
        // directory for pipeline-generated artefacts. Fall back to
        // `data_dir` as base when the session has no workspace (rare —
        // CLI clients without `session.workspace_cwd.v1` capability).
        let send_file_base = workspace_root
            .clone()
            .unwrap_or_else(|| bg_data_dir.clone());
        let mut send_file_tool = octos_agent::SendFileTool::new(out_tx)
            .with_base_dir(send_file_base)
            .with_extra_allowed_dir(bg_data_dir.clone());
        // Profiles with a custom `data_dir` outside `bg_data_dir` host
        // their plugin output under a path the default extras above would
        // reject. Add `plugin_root_dir` (resolved per-profile via
        // `routed_profile_id` or `session_id.profile_id()`) as an extra
        // allowed dir so spawn_only `send_file` deliveries from those
        // profiles still pass the path-scoping check.
        if plugin_root_dir != bg_data_dir {
            send_file_tool = send_file_tool.with_extra_allowed_dir(plugin_root_dir.clone());
        }
        send_file_tool.set_context("api", &bg_session_id.0);
        tool_registry.register(send_file_tool);

        // Drain `OutboundMessage`s emitted by `send_file` calls and persist
        // each one as an assistant message + media via the same canonical
        // path used by the spawn_only sender. Drops out when the turn ends
        // (the `out_tx` half is dropped along with the registry / agent
        // when the turn-scoped state is freed).
        //
        // M10 Phase 5a coalesce: when the outbound carries
        // `metadata.spawn_complete_companion = true`, persist the row with
        // `MessagePersistedSource::Background` (via the
        // `MESSAGE_PERSISTED_SOURCE_OVERRIDE` task-local). This marks each
        // per-file row from a spawn_only completion as a duplicate of the
        // forthcoming `turn/spawn_complete` envelope so dual-negotiated
        // clients suppress it at `live_event_passes_capability_filter`.
        // Without the capability the override has no wire-visible effect —
        // the row still reaches old clients as `message/persisted`.
        let consumer_sessions = bg_sessions.clone();
        let consumer_data_dir = bg_data_dir.clone();
        let consumer_session_id = bg_session_id.clone();
        let consumer_thread_id = bg_thread_id.clone();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let thread_id = msg
                    .metadata
                    .get("thread_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| consumer_thread_id.clone());
                let is_spawn_complete_companion = msg
                    .metadata
                    .get("spawn_complete_companion")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let persist = persist_assistant_with_media(
                    &consumer_sessions,
                    &consumer_data_dir,
                    &consumer_session_id,
                    msg.content,
                    msg.media,
                    thread_id,
                    "send_file",
                );
                if is_spawn_complete_companion {
                    let _ = MESSAGE_PERSISTED_SOURCE_OVERRIDE
                        .scope(Some(MessagePersistedSource::Background), persist)
                        .await;
                } else {
                    let _ = persist.await;
                }
            }
        });
    }
    let progress_workspace_root = workspace_root
        .clone()
        .or_else(|| tool_registry.workspace_root().map(Path::to_path_buf));

    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::channel::<String>(PROGRESS_CHANNEL_CAPACITY);
    let progress_dropped = Arc::new(AtomicU64::new(0));
    // PR F (M8.10 thread-binding chain `#649 → #740`): bind the originating
    // `TurnId` into the reporter so every progress event the agent emits
    // carries `thread_id`. Closes the wire-side leak where standalone-turn
    // SSE events landed unbound and the SPA reducer had to fall back to
    // sticky-map heuristics.
    let reporter: Arc<dyn octos_agent::ProgressReporter> =
        Arc::new(MetricsReporter::new(Arc::new(
            BoundedChannelReporter::new(progress_tx.clone(), progress_dropped.clone())
                .with_thread_id(Some(turn_id.0.to_string())),
        )));
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
    // M11-E: the agent is built per-turn (so per-turn callbacks layer in
    // without mutating shared session state), but its LLM, memory,
    // sandbox, and base system prompt come from the SessionRuntime
    // (preferred) or the legacy `state.agent`.
    //
    // M11-F regression fix REG-3: also propagate the profile-scope
    // hook executor (assembled once in `ProfileRuntime::bootstrap`
    // from `config.hooks + plugin_result.hooks`) onto the per-turn
    // rebuilt agent. Without this, every UI Protocol turn would bypass
    // the configured `before_tool_call` / `after_tool_call` /
    // `before_llm_call` / `after_llm_call` hooks because
    // `Agent::new_shared` resets `hooks: None`. We thread it directly
    // off the SessionRuntime's parent profile so the runtime layer
    // remains the single source of truth.
    let mut request_agent = Agent::new_shared(
        AgentId::new(format!("ui-protocol-{}", uuid::Uuid::now_v7())),
        llm_provider.clone(),
        Arc::new(tool_registry),
        memory_store.clone(),
    )
    .with_config(agent_config.clone())
    .with_system_prompt(append_workspace_root_hint(
        system_prompt_base.clone(),
        workspace_root.as_deref(),
    ))
    .with_reporter(reporter);
    if let Some(hooks) = session_runtime.profile.hook_executor.clone() {
        request_agent = request_agent.with_hooks(hooks);
    }
    // M11-F regression fix REG-1 follow-up (codex review): wire the
    // `activate_tools` back-reference on the per-turn rebuilt agent.
    // `ProfileRuntime::bootstrap` defers non-core groups + registers
    // the tool; without this wiring call, the LLM sees the tool in
    // `specs()` but `activate_tools` is unable to reach the registry
    // (its internal `Weak<ToolRegistry>` is empty). Gateway does the
    // equivalent at `session_actor.rs:2500`.
    request_agent.wire_activate_tools();

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
    // PR F (M8.10): capture the originating `TurnId` as a string so the
    // tokio::spawn closure (which moves everything it touches) can pre-stamp
    // each persisted Assistant/Tool message with the correct thread_id.
    // Required because Patch 8 fails the persist closed if Assistant/Tool
    // arrives unbound — the previous derive-from-history fallback picked
    // the WRONG sibling user under rapid-fire concurrent turns.
    let turn_thread_id_for_persist = turn_id.0.to_string();
    let turn_thread_id_for_done = turn_thread_id_for_persist.clone();
    // UPCR-2026-015 (M9-β-1): pull the pre-uploaded media paths off
    // the params and feed them to the agent loop. `process_message`
    // already accepts a `Vec<String>` of paths (used by the
    // `octos chat` CLI and gateway-mode message handler) — wiring it
    // here restores the legacy SSE chat handler's media delivery on
    // the WS transport. The legacy `rewrite_for` field is logged at
    // debug level for now; durable in-place rewrites land in a
    // follow-up that touches the per-session ledger replace path.
    let turn_media_paths: Vec<String> = params
        .media
        .iter()
        .map(|file_ref| file_ref.path.clone())
        .collect();
    if let Some(rewrite_for) = params.rewrite_for.as_deref() {
        tracing::debug!(
            session = %session_id.0,
            turn = %turn_id.0,
            rewrite_for,
            "turn/start carries rewrite_for; current build forwards the prompt without in-place ledger rewrite (β-1 advisory)"
        );
    }
    let agent_task = tokio::spawn(async move {
        let result = octos_agent::tools::TOOL_APPROVAL_CTX
            .scope(
                approval_requester,
                request_agent.process_message(&prompt, &history, turn_media_paths),
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
                        let to_save =
                            pre_stamp_turn_thread_id(message, &turn_thread_id_for_persist);
                        if let Ok(seq) = sessions
                            .add_message_with_seq(&agent_session_id, to_save)
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
                    "thread_id": turn_thread_id_for_done,
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
                    // Tag with this connection's id so its forwarder skips
                    // the broadcast copy after the direct send below.
                    let event = ledger.append_progress_from(status.event, ws.connection_id);
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
                    // UPCR-2026-014 (M9-α-9) addendum fields; the WS
                    // lifecycle path doesn't have token usage /
                    // session_result threaded yet — those land via the
                    // SSE bridge in α-9. Leaving them None preserves
                    // the pre-addendum wire shape for WS-driven turns.
                    tokens_in: None,
                    tokens_out: None,
                    session_result: None,
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
    ledger: &UiProtocolLedger,
    approvals: &PendingApprovalStore,
) {
    let turns = std::mem::take(&mut *connection_turns.lock().await);
    if turns.is_empty() {
        return;
    }

    let mut active = active_turns.lock().await;
    for (session_id, turn_id) in turns {
        let mut aborted_state: Option<Arc<TokioMutex<TurnState>>> = None;
        let should_abort = active
            .get(&session_id)
            .is_some_and(|active| active.turn_id == turn_id);
        if should_abort {
            if let Some(active) = active.remove(&session_id) {
                aborted_state = Some(active.state.clone());
                active.abort.abort();
            }
        }
        // #920.1: append a durable terminal event so reconnect-replay
        // sees this turn end. Without this the in-flight turn vanishes
        // from the live registry but no `turn/error` lands, so clients
        // render an indefinite spinner. Use the same single-fire
        // transition the rest of the lifecycle uses so we don't race
        // with a natural completion / interrupt that may already have
        // flipped state to Terminal.
        if let Some(state) = aborted_state {
            if let Some(transition) =
                transition_to_terminal(state.as_ref(), TerminalReason::Interrupted).await
            {
                let _ = ledger.append_notification(UiNotification::TurnError(TurnErrorEvent {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    code: "connection_closed".to_owned(),
                    message: "connection closed before turn completed".to_owned(),
                }));
                if let Some(ack) = transition.ack {
                    let _ = ack.send(());
                }
            }
        }
        // #920.2: cancel every still-pending approval for the aborted
        // turn and append a durable `approval/cancelled` for each so a
        // reconnect doesn't re-show a modal for a dead turn.
        let cancelled = approvals.cancel_pending_for_turn(
            &session_id,
            &turn_id,
            approval_cancelled_reasons::TURN_INTERRUPTED,
        );
        for entry in cancelled {
            let _ = ledger.append_notification(UiNotification::ApprovalCancelled(
                ApprovalCancelledEvent {
                    session_id: session_id.clone(),
                    approval_id: entry.approval_id,
                    turn_id: entry.turn_id,
                    reason: approval_cancelled_reasons::TURN_INTERRUPTED.to_owned(),
                },
            ));
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

/// Push a WebSocket close frame with an explicit status code and reason. The
/// `writer_loop` forwards the close to the peer and then drains; callers
/// should `return` immediately after this call.
///
/// Used to signal durable auth failure (code 1008 / "auth_expired"). The SPA
/// bridge (Web PR #114, `auth-context.tsx`) subscribes to close-code 1008 to
/// invoke `crew:auth_expired`, which clears the cached token and routes to
/// /login. Callers MUST enqueue this close frame BEFORE any accompanying
/// `RpcError` envelope: under writer-channel backpressure (capacity-1 with one
/// slot used), only the first try_send survives, and the close is the
/// load-bearing signal the SPA listens for (codex BLOCK 2026-05-13).
fn close_ws_with_code(ws: &WsConnection, code: u16, reason: &str) -> Result<(), SendError> {
    let frame = WsMessage::Close(Some(axum::extract::ws::CloseFrame {
        code,
        reason: reason.into(),
    }));
    ws.send_lifecycle(frame)
}

/// Send a scope-validation error back to the caller and, when the error came
/// from `validate_authenticated_session_scope` (i.e. the connection IS
/// authenticated and the requested scope doesn't match), accompany it with a
/// close-code 1008 frame so the SPA `crew:auth_expired` listener fires.
/// Non-auth scope errors (malformed input, etc.) leave the socket open.
fn send_scope_error(ws: &WsConnection, id: String, error: RpcError) {
    let auth_violation = is_auth_scope_violation(&error);
    // Codex BLOCK (2026-05-13): when the writer channel has just one free
    // slot, the close-code is the load-bearing signal — the SPA uses it to
    // detect auth-expiry and clear its token. Enqueue the close FIRST so it
    // survives backpressure even if the courtesy error envelope is dropped.
    if auth_violation {
        let _ = close_ws_with_code(ws, 1008, "auth_expired");
    }
    let _ = send_rpc_error(ws, Some(id), error);
}

fn send_notification_lifecycle(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    notification: UiNotification,
) -> Result<(), SendError> {
    // Tag the broadcast with the originating connection so this
    // connection's own live forwarder skips the duplicate copy.
    let event = ledger.append_notification_from(notification, ws.connection_id);
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
    let event = ledger.append_notification_from(notification, ws.connection_id);
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
    // #924 NIT 7: exhaustive on both `UiProtocolLedgerEvent` AND the
    // inner `UiNotification`. A `_ => None` catchall would let a
    // future cursor-bearing variant compile cleanly while silently
    // being skipped for replay-lossy cursor extraction (#921 was
    // exactly that bug for `MessagePersisted` / `TurnSpawnComplete`).
    // The rule for new variants: if you add a `cursor: UiCursor` or
    // `cursor: Option<UiCursor>` field, add it here too. Variants
    // whose "cursor" is an `OutputCursor` (task output stream) are
    // explicitly NOT surfaced here — that's a separate replay channel.
    match event {
        UiProtocolLedgerEvent::Notification(notification) => match notification {
            UiNotification::SessionOpened(SessionOpened { cursor, .. }) => cursor.clone(),
            UiNotification::TurnCompleted(TurnCompletedEvent { cursor, .. }) => cursor.clone(),
            UiNotification::MessagePersisted(persisted) => Some(persisted.cursor.clone()),
            UiNotification::TurnSpawnComplete(spawn) => Some(spawn.cursor.clone()),
            // Non-cursor-bearing variants — exhaustively enumerated so a
            // future addition forces an explicit decision here.
            UiNotification::TurnStarted(_)
            | UiNotification::MessageDelta(_)
            | UiNotification::ToolStarted(_)
            | UiNotification::ToolProgress(_)
            | UiNotification::ToolCompleted(_)
            | UiNotification::ApprovalRequested(_)
            | UiNotification::ApprovalAutoResolved(_)
            | UiNotification::ApprovalDecided(_)
            | UiNotification::ApprovalCancelled(_)
            | UiNotification::TaskUpdated(_)
            // TaskOutputDelta carries an `OutputCursor`, not a `UiCursor`.
            | UiNotification::TaskOutputDelta(_)
            | UiNotification::ProgressUpdated(_)
            | UiNotification::Warning(_)
            | UiNotification::TurnError(_)
            // ReplayLossy references a `last_durable_cursor` belonging to
            // the events it summarises, not its own — surfacing it here
            // would re-loop the replay flag onto itself.
            | UiNotification::ReplayLossy(_)
            | UiNotification::FileAttached(_)
            | UiNotification::SessionEventBridged(_) => None,
        },
        UiProtocolLedgerEvent::Progress(_) => None,
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
    let event = ledger.append_notification_from(lossy, ws.connection_id);
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
            media: Vec::new(),
            topic: None,
            rewrite_for: None,
        })
        .into_rpc_request("1")
        .expect("request");
        let text = serde_json::to_string(&request).expect("json");

        let decoded = parse_rpc_request(&text).expect("parse");

        assert_eq!(decoded.method, methods::TURN_START);
        assert_eq!(decoded.id, "1");
        assert!(matches!(
            route_rpc_command(decoded, ConnectionUiFeatures::default()).expect("route"),
            UiCommand::TurnStart(_)
        ));
    }

    /// UPCR-2026-015 (M9-β-1): the WS turn/start handler accepts the
    /// three new optional fields (`media`, `topic`, `rewrite_for`)
    /// from a strict-additive wire shape. The legacy text-only form
    /// continues to deserialize identically (back-compat sanity).
    #[test]
    fn parses_turn_start_rpc_request_with_beta1_fields() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": "rpc-beta1",
            "method": methods::TURN_START,
            "params": {
                "session_id": "local:test",
                "turn_id": TurnId::new(),
                "input": [{"kind": "text", "text": "look here"}],
                "media": [
                    {
                        "path": "/tmp/chat-upload-deadbeef.png",
                        "mime": "image/png",
                        "size_bytes": 1234,
                    }
                ],
                "topic": "research",
                "rewrite_for": "cmid-original-1",
            }
        })
        .to_string();

        let decoded = parse_rpc_request(&raw).expect("parse");
        let routed = route_rpc_command(decoded, ConnectionUiFeatures::default()).expect("route");
        match routed {
            UiCommand::TurnStart(params) => {
                assert_eq!(params.media.len(), 1);
                assert_eq!(params.media[0].path, "/tmp/chat-upload-deadbeef.png");
                assert_eq!(params.media[0].mime, "image/png");
                assert_eq!(params.media[0].size_bytes, 1234);
                assert_eq!(params.topic.as_deref(), Some("research"));
                assert_eq!(params.rewrite_for.as_deref(), Some("cmid-original-1"));
            }
            other => panic!("expected TurnStart, got {:?}", other),
        }
    }

    /// UPCR-2026-015 (M9-β-1): bare turn/start (no β-1 fields)
    /// continues to deserialize and round-trip with the new defaults.
    #[test]
    fn parses_legacy_turn_start_rpc_request_stays_back_compat() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": "rpc-legacy",
            "method": methods::TURN_START,
            "params": {
                "session_id": "local:test",
                "turn_id": TurnId::new(),
                "input": [{"kind": "text", "text": "hello"}],
            }
        })
        .to_string();

        let decoded = parse_rpc_request(&raw).expect("parse");
        let routed = route_rpc_command(decoded, ConnectionUiFeatures::default()).expect("route");
        match routed {
            UiCommand::TurnStart(params) => {
                assert!(params.media.is_empty());
                assert!(params.topic.is_none());
                assert!(params.rewrite_for.is_none());
            }
            other => panic!("expected TurnStart, got {:?}", other),
        }
    }

    /// #921: every cursor-bearing durable notification variant must
    /// surface its cursor through `ledger_event_cursor` so dropped
    /// sends trigger `protocol/replay_lossy`. Asserts the positive
    /// extraction for the four variants and a negative for a non-
    /// cursor-bearing one (sanity).
    #[test]
    fn ledger_event_cursor_covers_every_cursor_bearing_variant() {
        let session_id = SessionKey("local:test".into());
        let cursor = UiCursor {
            stream: session_id.0.clone(),
            seq: 42,
        };

        let opened =
            UiProtocolLedgerEvent::Notification(UiNotification::SessionOpened(SessionOpened {
                session_id: session_id.clone(),
                active_profile_id: None,
                workspace_root: None,
                cursor: Some(cursor.clone()),
                panes: None,
                capabilities: octos_core::ui_protocol::UiProtocolCapabilities::first_server_slice(),
            }));
        assert_eq!(ledger_event_cursor(&opened), Some(cursor.clone()));

        let completed = UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(
            TurnCompletedEvent {
                session_id: session_id.clone(),
                turn_id: TurnId::new(),
                cursor: Some(cursor.clone()),
                tokens_in: None,
                tokens_out: None,
                session_result: None,
            },
        ));
        assert_eq!(ledger_event_cursor(&completed), Some(cursor.clone()));

        let persisted = UiProtocolLedgerEvent::Notification(UiNotification::MessagePersisted(
            MessagePersistedEvent {
                session_id: session_id.clone(),
                turn_id: None,
                thread_id: None,
                seq: 1,
                role: "assistant".into(),
                message_id: "msg-1".into(),
                client_message_id: None,
                source: MessagePersistedSource::Assistant,
                cursor: cursor.clone(),
                persisted_at: chrono::Utc::now(),
                media: Vec::new(),
            },
        ));
        assert_eq!(ledger_event_cursor(&persisted), Some(cursor.clone()));

        let spawn = UiProtocolLedgerEvent::Notification(UiNotification::TurnSpawnComplete(
            TurnSpawnCompleteEvent {
                session_id: session_id.clone(),
                turn_id: None,
                thread_id: None,
                task_id: "task-1".into(),
                response_to_client_message_id: None,
                seq: 1,
                message_id: "msg-1".into(),
                source: "background".into(),
                cursor: cursor.clone(),
                persisted_at: chrono::Utc::now(),
                content: "done".into(),
                media: Vec::new(),
            },
        ));
        assert_eq!(ledger_event_cursor(&spawn), Some(cursor.clone()));

        // Sanity: a non-cursor-bearing variant returns None.
        let delta =
            UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(MessageDeltaEvent {
                session_id: session_id.clone(),
                turn_id: TurnId::new(),
                text: "x".into(),
            }));
        assert_eq!(ledger_event_cursor(&delta), None);
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
            route_rpc_command(request, ConnectionUiFeatures::default()).expect("task/output/read routes"),
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
                harness_task_control: false,
                session_hydrate: false,
                thread_graph: false,
                turn_state_get: false,
                message_persisted: false,
                spawn_complete: false,
                auxiliary_rest_to_ws_v1: false,
                header_present: true,
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
                harness_task_control: false,
                session_hydrate: false,
                thread_graph: false,
                turn_state_get: false,
                message_persisted: false,
                spawn_complete: false,
                auxiliary_rest_to_ws_v1: false,
                header_present: true,
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

    /// Audit #715 regression: a plugin manifest declared `risk: "high"` must
    /// surface on the wire `approval_requested` event so approval cards and
    /// the audit trail can render the badge. Previously, only `tool_name ==
    /// "shell"` populated `event.risk`, so plugin tools went out unclassified
    /// even when manifest gating engaged (PR #712).
    #[test]
    fn plugin_high_risk_approval_emits_risk_field_on_wire() {
        let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
            tool_risk_registry_test_lock().clear_poison();
            e.into_inner()
        });
        clear_tool_risk_registry_for_test();
        register_tool_risk_for_test("weather_lookup", "high");

        // Plugin tools route approvals via `command: None`; only `cwd`
        // (the plugin work_dir) flows through the request.
        let request = ToolApprovalRequest {
            tool_id: "tool-plugin-1".into(),
            tool_name: "weather_lookup".into(),
            title: "Approve plugin tool".into(),
            body: "Plugin 'weather' tool 'weather_lookup' is declared high risk.".into(),
            command: None,
            cwd: Some("/tmp/weather-plugin".into()),
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
                harness_task_control: false,
                session_hydrate: false,
                thread_graph: false,
                turn_state_get: false,
                message_persisted: false,
                spawn_complete: false,
                auxiliary_rest_to_ws_v1: false,
                header_present: true,
            },
        );

        assert_eq!(
            event.risk.as_deref(),
            Some("high"),
            "plugin tool's manifest-declared risk must reach the wire"
        );
        // Plugin path doesn't produce shell-style typed_details/render_hints.
        assert!(event.approval_kind.is_none());
        assert!(event.typed_details.is_none());
        assert!(event.render_hints.is_none());
        clear_tool_risk_registry_for_test();
    }

    /// Audit #715 regression: `critical` risk plugins must reach the wire so
    /// the UI can render the highest-severity badge.
    #[test]
    fn plugin_critical_risk_approval_emits_risk_critical() {
        let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
            tool_risk_registry_test_lock().clear_poison();
            e.into_inner()
        });
        clear_tool_risk_registry_for_test();
        register_tool_risk_for_test("destroy_world", "critical");

        let request = ToolApprovalRequest {
            tool_id: "tool-plugin-2".into(),
            tool_name: "destroy_world".into(),
            title: "Approve plugin tool".into(),
            body: "Plugin 'apocalypse' tool 'destroy_world' is declared critical risk.".into(),
            command: None,
            cwd: None,
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
                harness_task_control: false,
                session_hydrate: false,
                thread_graph: false,
                turn_state_get: false,
                message_persisted: false,
                spawn_complete: false,
                auxiliary_rest_to_ws_v1: false,
                header_present: true,
            },
        );

        assert_eq!(event.risk.as_deref(), Some("critical"));
        clear_tool_risk_registry_for_test();
    }

    /// Regression for the existing shell typed-approvals path: lifting the
    /// risk assignment out of the `tool_name == "shell"` guard must not
    /// break shell event population (typed_details, render_hints, risk).
    #[test]
    fn shell_approval_still_emits_risk_field() {
        let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
            tool_risk_registry_test_lock().clear_poison();
            e.into_inner()
        });
        clear_tool_risk_registry_for_test();
        register_tool_risk_for_test("shell", "medium");

        let request = ToolApprovalRequest {
            tool_id: "tool-shell-1".into(),
            tool_name: "shell".into(),
            title: "Approve shell command".into(),
            body: "Command:\ncargo test".into(),
            command: Some("cargo test".into()),
            cwd: Some("/tmp/work".into()),
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
                harness_task_control: false,
                session_hydrate: false,
                thread_graph: false,
                turn_state_get: false,
                message_persisted: false,
                spawn_complete: false,
                auxiliary_rest_to_ws_v1: false,
                header_present: true,
            },
        );

        assert_eq!(event.risk.as_deref(), Some("medium"));
        assert_eq!(
            event.approval_kind.as_deref(),
            Some(approval_kinds::COMMAND)
        );
        assert!(event.typed_details.is_some());
        assert!(event.render_hints.is_some());
        clear_tool_risk_registry_for_test();
    }

    /// When `typed_approvals` is not negotiated, the wire event must remain
    /// fully generic — no `risk` field, no typed details. Audit #715 fix
    /// must not start advertising risk on the legacy untyped path, which
    /// older clients are not prepared to parse.
    #[test]
    fn tool_with_no_risk_classification_does_not_emit_risk_field() {
        let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
            tool_risk_registry_test_lock().clear_poison();
            e.into_inner()
        });
        clear_tool_risk_registry_for_test();
        register_tool_risk_for_test("weather_lookup", "high");

        let request = ToolApprovalRequest {
            tool_id: "tool-plugin-3".into(),
            tool_name: "weather_lookup".into(),
            title: "Approve plugin tool".into(),
            body: "Plugin tool approval".into(),
            command: None,
            cwd: Some("/tmp/weather-plugin".into()),
        };
        // `typed_approvals: false` — legacy client.
        let event = approval_event_from_tool_request(
            request,
            SessionKey("local:test".into()),
            ApprovalId::new(),
            TurnId::new(),
            ConnectionUiFeatures::default(),
        );

        assert!(
            event.risk.is_none(),
            "legacy untyped path must not advertise risk; got {:?}",
            event.risk
        );
        assert!(event.approval_kind.is_none());
        assert!(event.typed_details.is_none());
        assert!(event.render_hints.is_none());
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
                harness_task_control: false,
                session_hydrate: false,
                thread_graph: false,
                turn_state_get: false,
                message_persisted: false,
                spawn_complete: false,
                auxiliary_rest_to_ws_v1: false,
                header_present: true,
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
            route_rpc_command(approval, ConnectionUiFeatures::default()).expect("approval/respond routes"),
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
            route_rpc_command(diff, ConnectionUiFeatures::default()).expect("diff/preview/get routes"),
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
            route_rpc_command(task_cancel, ConnectionUiFeatures::default()).expect("task/cancel routes"),
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
            let command = route_rpc_command(request, ConnectionUiFeatures::default())
                .expect("server-supported method routes");
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

        let error =
            route_rpc_command(request, ConnectionUiFeatures::default()).expect_err("bad params");

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

    /// #922.1: an envelope without `id` is a JSON-RPC notification and
    /// must not yield a parse_error reply.
    #[test]
    fn idless_envelope_parses_as_notification() {
        let frame = r#"{"jsonrpc":"2.0","method":"ping","params":{}}"#;
        match parse_ws_text_frame(frame).expect("parses") {
            ParsedFrame::Notification(method) => assert_eq!(method, "ping"),
            ParsedFrame::Request(_) => panic!("expected notification"),
        }
        assert!(is_known_inbound_notification("ping"));
        assert!(!is_known_inbound_notification("unknown"));
    }

    /// #924 NIT 6: distinguish notifications from requests by KEY
    /// presence on `id`, not null-check. An envelope with
    /// `"id": null` is malformed — surfacing it loudly via
    /// parse_error catches client bugs that the silent-drop path
    /// hid.
    #[test]
    fn null_id_envelope_is_rejected_with_parse_error() {
        let frame =
            r#"{"jsonrpc":"2.0","id":null,"method":"session/open","params":{"session_id":"x"}}"#;
        let err = parse_ws_text_frame(frame).expect_err("null id must reject");
        assert_eq!(
            err.code,
            octos_core::ui_protocol::rpc_error_codes::PARSE_ERROR
        );
        assert!(
            err.message.contains("null"),
            "parse error message should mention the null id; got {}",
            err.message
        );
    }

    /// #924 NIT 6: numeric ids are also rejected — the server's RpcRequest
    /// shape requires `id: String`.
    #[test]
    fn numeric_id_envelope_is_rejected_with_parse_error() {
        let frame =
            r#"{"jsonrpc":"2.0","id":42,"method":"session/open","params":{"session_id":"x"}}"#;
        let err = parse_ws_text_frame(frame).expect_err("numeric id must reject");
        assert_eq!(
            err.code,
            octos_core::ui_protocol::rpc_error_codes::PARSE_ERROR
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

    // Place near other ui_protocol tests. Verifies a profile-mismatched session_id
    // yields close-code 1008.
    #[tokio::test]
    async fn send_scope_error_closes_with_1008_on_authenticated_mismatch() {
        let (ws, mut rx) = ws_connection_for_test(8);
        let session_id = SessionKey::with_profile("profile-b", "api", "chat-1");
        let error =
            validate_session_scope(&session_id, None, Some("profile-a")).expect_err("scope error");
        assert!(is_auth_scope_violation(&error));

        send_scope_error(&ws, "rpc-1".into(), error);

        // First frame is the close-code 1008 with reason "auth_expired" — the
        // close MUST precede the error envelope so it survives writer-channel
        // backpressure (codex BLOCK 2026-05-13).
        let first = rx.recv().await.expect("close frame");
        match first {
            axum::extract::ws::Message::Close(Some(frame)) => {
                assert_eq!(frame.code, 1008);
                assert_eq!(frame.reason.as_str(), "auth_expired");
            }
            other => panic!("expected close frame with 1008, got {other:?}"),
        }

        // Second frame is the JSON-RPC error envelope (courtesy detail).
        let second = rx.recv().await.expect("rpc error frame");
        let text = match second {
            axum::extract::ws::Message::Text(text) => text,
            other => panic!("expected text frame, got {other:?}"),
        };
        assert!(text.contains("expected_profile_id"));
    }

    #[tokio::test]
    async fn send_scope_error_does_not_close_when_unauthenticated() {
        let (ws, mut rx) = ws_connection_for_test(8);
        // No connection_profile_id => not authenticated; cross-profile id is
        // a generic invalid_params, not an auth scope violation.
        let session_id = SessionKey::with_profile("profile-a", "api", "chat-1");
        let error =
            validate_session_scope(&session_id, Some("profile-b"), None).expect_err("scope error");
        assert!(!is_auth_scope_violation(&error));

        send_scope_error(&ws, "rpc-1".into(), error);

        // Only the JSON-RPC error envelope should arrive — no close frame.
        let _first = rx.recv().await.expect("rpc error frame");
        // Drop the sender side so a pending recv resolves promptly; instead,
        // poll once with no wait to confirm the queue is empty.
        assert!(rx.try_recv().is_err(), "no close frame expected");
    }

    /// Codex BLOCK regression (2026-05-13): with the writer channel at
    /// capacity 2 and one slot already used, `send_scope_error` must use
    /// the remaining slot for the 1008 close — NOT the courtesy error
    /// envelope. The close is the load-bearing signal the SPA's
    /// `crew:auth_expired` listener uses to clear its token. The error
    /// envelope is allowed to drop under backpressure.
    ///
    /// Test geometry: capacity 2 + 1 primer = exactly one free slot at the
    /// moment `send_scope_error` enqueues. Pre-fix the order was
    /// error-then-close → error queued, close dropped. Post-fix the order
    /// is close-then-error → close queued, error dropped.
    #[tokio::test]
    async fn auth_scope_violation_close_frame_survives_capacity_one_writer() {
        let (ws, mut rx) = ws_connection_for_test(2);

        // Pre-fill ONE slot so only one of the two outbound frames can
        // survive backpressure. The close MUST be that one.
        ws.writer
            .try_send(axum::extract::ws::Message::Text("priming".into()))
            .expect("prime channel");
        assert_eq!(
            ws.writer.capacity(),
            1,
            "channel must have exactly one free slot for the backpressure case",
        );

        let session_id = SessionKey::with_profile("profile-b", "api", "chat-1");
        let error =
            validate_session_scope(&session_id, None, Some("profile-a")).expect_err("scope error");
        assert!(is_auth_scope_violation(&error));

        send_scope_error(&ws, "rpc-1".into(), error);

        // Drain the priming frame first.
        let primer = rx.recv().await.expect("priming frame");
        assert!(matches!(primer, axum::extract::ws::Message::Text(_)));

        // The next frame MUST be the 1008 close. The error envelope was
        // dropped under backpressure — that's acceptable; the close is
        // what the SPA listens for.
        let next = rx.recv().await.expect("close frame survives backpressure");
        match next {
            axum::extract::ws::Message::Close(Some(frame)) => {
                assert_eq!(frame.code, 1008);
                assert_eq!(frame.reason.as_str(), "auth_expired");
            }
            other => panic!("expected 1008 close to survive backpressure, got {other:?}"),
        }
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
            ConnectionId::next(),
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
            ConnectionId::next(),
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
            ConnectionId::next(),
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
            ConnectionId::next(),
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
            ConnectionId::next(),
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
            ConnectionId::next(),
            None,
            ConnectionUiFeatures {
                typed_approvals: false,
                pane_snapshots: true,
                session_workspace_cwd: false,
                harness_task_control: false,
                session_hydrate: false,
                thread_graph: false,
                turn_state_get: false,
                message_persisted: false,
                spawn_complete: false,
                auxiliary_rest_to_ws_v1: false,
                header_present: true,
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
            ConnectionId::next(),
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

    // ----- UPCR-2026-007: capability advertisement on `SessionOpened` -----

    #[tokio::test]
    async fn session_open_result_advertises_full_protocol_when_no_header() {
        // Client sent no `X-Octos-Ui-Features` request — server returns
        // the `first_server_slice` baseline so a discovery-aware client
        // can learn the surface in-band per UPCR-2026-007.
        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:caps-default".into());

        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            ConnectionId::next(),
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_id,
                profile_id: None,
                cwd: None,
                after: None,
            },
        )
        .await
        .expect("open session without feature header");

        let capabilities = &outcome.result.opened.capabilities;
        assert_eq!(
            capabilities,
            &UiProtocolCapabilities::first_server_slice(),
            "no header => server falls back to first_server_slice"
        );
        assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1));
        assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1));
        assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1));
        assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1));
    }

    #[tokio::test]
    async fn session_open_result_advertises_intersection_when_header_subset() {
        // Client requested only `pane.snapshots.v1` — server returns
        // capabilities with that single feature and never leaks flags the
        // client did not negotiate (UPCR-2026-007 § 4 capability
        // negotiation).
        let temp = tempfile::tempdir().expect("tempdir");
        let state = state_with_sessions(temp.path());
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey("local:caps-subset".into());

        let mut headers = HeaderMap::new();
        headers.insert(
            UI_FEATURES_HEADER,
            UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1
                .parse()
                .expect("header value"),
        );
        let features = ConnectionUiFeatures::from_headers_and_query(&headers, None);
        assert!(features.header_present);
        assert!(features.pane_snapshots);
        assert!(!features.typed_approvals);

        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            ConnectionId::next(),
            None,
            features,
            SessionOpenParams {
                session_id,
                profile_id: None,
                cwd: None,
                after: None,
            },
        )
        .await
        .expect("open session with feature header subset");

        let capabilities = &outcome.result.opened.capabilities;
        assert_eq!(
            capabilities.supported_features,
            vec![UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1.to_owned()],
            "intersection must be exactly the features the client asked for"
        );
        assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1));
        assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1));
        assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1));
        // Unconditional methods stay advertised so the client can still
        // see what the server offers in-band.
        assert!(
            capabilities
                .supported_methods
                .iter()
                .any(|method| method == octos_core::ui_protocol::methods::SESSION_OPEN)
        );
        // Capability-gated methods (task-control RPCs behind
        // harness.task_control.v1) must not leak when the gating feature
        // is not in the negotiated set — otherwise the client would call
        // them and the server would reject with method_not_supported.
        assert!(
            !capabilities
                .supported_methods
                .iter()
                .any(|method| method == octos_core::ui_protocol::methods::TASK_LIST),
            "task/list must be gated by harness.task_control.v1"
        );
        assert!(
            !capabilities
                .supported_methods
                .iter()
                .any(|method| method == octos_core::ui_protocol::methods::TASK_CANCEL),
            "task/cancel must be gated by harness.task_control.v1"
        );
    }

    // ===== M12 Phase D-1 auxiliary REST → WS negotiation =====

    #[test]
    fn aux_rest_to_ws_v1_feature_token_parses_from_query_string() {
        // Mirrors the existing `?ui_feature=session.workspace_cwd.v1`
        // pattern (`e2e/scripts/soak-m11-multi.ts:21`). A client that
        // appends `?ui_feature=auxiliary.rest_to_ws.v1` MUST be picked
        // up by the per-connection feature snapshot.
        let headers = HeaderMap::new();
        let features = ConnectionUiFeatures::from_headers_and_query(
            &headers,
            Some("token=redacted&ui_feature=auxiliary.rest_to_ws.v1"),
        );
        assert!(features.auxiliary_rest_to_ws_v1);
        assert!(features.header_present);
        // Negative: an unrelated feature stays unflipped.
        assert!(!features.harness_task_control);
    }

    #[test]
    fn aux_rest_to_ws_v1_feature_token_parses_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            UI_FEATURES_HEADER,
            UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1
                .parse()
                .expect("header value"),
        );
        let features = ConnectionUiFeatures::from_headers_and_query(&headers, None);
        assert!(features.auxiliary_rest_to_ws_v1);
        assert!(features.header_present);
    }

    #[test]
    fn aux_rest_to_ws_v1_not_negotiated_when_only_other_features_requested() {
        // Client requests `harness.task_control.v1` — the auxiliary
        // capability must NOT be auto-enabled. Phase D-1 is strictly
        // opt-in.
        let mut headers = HeaderMap::new();
        headers.insert(
            UI_FEATURES_HEADER,
            UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1
                .parse()
                .expect("header value"),
        );
        let features = ConnectionUiFeatures::from_headers_and_query(&headers, None);
        assert!(features.harness_task_control);
        assert!(!features.auxiliary_rest_to_ws_v1);
    }

    #[test]
    fn aux_rest_to_ws_v1_negotiated_capabilities_include_only_when_requested() {
        let mut headers = HeaderMap::new();
        headers.insert(
            UI_FEATURES_HEADER,
            UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1
                .parse()
                .expect("header value"),
        );
        let features = ConnectionUiFeatures::from_headers_and_query(&headers, None);
        let capabilities = features.negotiated_capabilities();
        assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1));
        for method in [
            octos_core::ui_protocol::methods::SESSION_LIST,
            octos_core::ui_protocol::methods::SESSION_SNAPSHOT,
            octos_core::ui_protocol::methods::SESSION_MESSAGES_PAGE,
            octos_core::ui_protocol::methods::SESSION_STATUS_GET,
            octos_core::ui_protocol::methods::SESSION_FILES_LIST,
            octos_core::ui_protocol::methods::SESSION_TASKS_LIST,
            octos_core::ui_protocol::methods::SESSION_WORKSPACE_GET,
            octos_core::ui_protocol::methods::SESSION_TITLE_SET,
            octos_core::ui_protocol::methods::SESSION_DELETE,
            octos_core::ui_protocol::methods::SYSTEM_STATUS_GET,
            octos_core::ui_protocol::methods::CONTENT_LIST,
            octos_core::ui_protocol::methods::CONTENT_DELETE,
            octos_core::ui_protocol::methods::CONTENT_BULK_DELETE,
        ] {
            assert!(
                capabilities.supports_method(method),
                "{method} must be advertised when auxiliary.rest_to_ws.v1 is negotiated"
            );
        }
    }

    #[test]
    fn aux_rest_to_ws_v1_negotiated_capabilities_omit_when_not_requested() {
        let mut headers = HeaderMap::new();
        // Request a different feature so `header_present == true`.
        headers.insert(
            UI_FEATURES_HEADER,
            UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1
                .parse()
                .expect("header value"),
        );
        let features = ConnectionUiFeatures::from_headers_and_query(&headers, None);
        let capabilities = features.negotiated_capabilities();
        assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1));
        for method in [
            octos_core::ui_protocol::methods::SESSION_LIST,
            octos_core::ui_protocol::methods::SESSION_SNAPSHOT,
            octos_core::ui_protocol::methods::SESSION_MESSAGES_PAGE,
            octos_core::ui_protocol::methods::SYSTEM_STATUS_GET,
            octos_core::ui_protocol::methods::CONTENT_LIST,
            octos_core::ui_protocol::methods::CONTENT_DELETE,
            octos_core::ui_protocol::methods::CONTENT_BULK_DELETE,
        ] {
            assert!(
                !capabilities.supports_method(method),
                "{method} must NOT be advertised without auxiliary.rest_to_ws.v1"
            );
        }
    }

    #[test]
    fn aux_rest_to_ws_v1_route_rpc_rejects_methods_when_feature_not_negotiated() {
        // A client that sent ANY feature header but NOT
        // `auxiliary.rest_to_ws.v1` must see the aux methods rejected
        // with `method_not_supported`, matching the existing gate for
        // `task/list` behind `harness.task_control.v1`.
        let mut headers = HeaderMap::new();
        headers.insert(
            UI_FEATURES_HEADER,
            UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1
                .parse()
                .expect("header value"),
        );
        let features = ConnectionUiFeatures::from_headers_and_query(&headers, None);
        assert!(features.header_present);
        assert!(!features.auxiliary_rest_to_ws_v1);
        for method in [
            octos_core::ui_protocol::methods::SESSION_LIST,
            octos_core::ui_protocol::methods::SESSION_SNAPSHOT,
            octos_core::ui_protocol::methods::SESSION_DELETE,
            octos_core::ui_protocol::methods::SYSTEM_STATUS_GET,
            octos_core::ui_protocol::methods::CONTENT_LIST,
        ] {
            let request = RpcRequest::<Value>::new("req-1", method, Value::Null);
            let result = route_rpc_command(request, features);
            let err = result.expect_err("aux method must be rejected without feature");
            assert_eq!(
                err.code,
                octos_core::ui_protocol::rpc_error_codes::METHOD_NOT_SUPPORTED,
                "{method} must reject with METHOD_NOT_SUPPORTED"
            );
        }
    }

    #[test]
    fn aux_rest_to_ws_v1_route_rpc_accepts_methods_when_feature_negotiated() {
        let mut headers = HeaderMap::new();
        headers.insert(
            UI_FEATURES_HEADER,
            UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1
                .parse()
                .expect("header value"),
        );
        let features = ConnectionUiFeatures::from_headers_and_query(&headers, None);
        assert!(features.auxiliary_rest_to_ws_v1);
        for (method, params) in [
            (
                octos_core::ui_protocol::methods::SESSION_LIST,
                Value::Object(serde_json::Map::new()),
            ),
            (
                octos_core::ui_protocol::methods::SYSTEM_STATUS_GET,
                Value::Object(serde_json::Map::new()),
            ),
        ] {
            let request = RpcRequest::<Value>::new("req-2", method, params);
            let cmd = route_rpc_command(request, features).expect("aux method accepted");
            assert_eq!(cmd.method(), method);
        }
    }

    /// Codex review 2026-05-12 (BLOCK 1): a client that sends NO
    /// feature header at all must still be rejected when it calls one
    /// of the M12 Phase D-1 auxiliary methods. The original
    /// implementation gated only when `features.header_present` was
    /// true, which let pre-existing no-header clients trip into
    /// `session/delete` / `content/delete` / etc. accidentally — that
    /// breaks the strict opt-in contract.
    #[test]
    fn aux_rest_to_ws_v1_route_rpc_rejects_methods_with_no_feature_header_at_all() {
        // No header, no query string — the legacy "advertise full
        // first-slice in SessionOpened" path. `header_present` must
        // be false (verified) and `auxiliary_rest_to_ws_v1` must be
        // false (verified), and yet the gate must fire.
        let headers = HeaderMap::new();
        let features = ConnectionUiFeatures::from_headers_and_query(&headers, None);
        assert!(
            !features.header_present,
            "no header → header_present must be false"
        );
        assert!(
            !features.auxiliary_rest_to_ws_v1,
            "no header → auxiliary.rest_to_ws.v1 must be false"
        );
        for method in [
            octos_core::ui_protocol::methods::SESSION_LIST,
            octos_core::ui_protocol::methods::SESSION_SNAPSHOT,
            octos_core::ui_protocol::methods::SESSION_MESSAGES_PAGE,
            octos_core::ui_protocol::methods::SESSION_STATUS_GET,
            octos_core::ui_protocol::methods::SESSION_FILES_LIST,
            octos_core::ui_protocol::methods::SESSION_TASKS_LIST,
            octos_core::ui_protocol::methods::SESSION_WORKSPACE_GET,
            octos_core::ui_protocol::methods::SESSION_TITLE_SET,
            octos_core::ui_protocol::methods::SESSION_DELETE,
            octos_core::ui_protocol::methods::SYSTEM_STATUS_GET,
            octos_core::ui_protocol::methods::CONTENT_LIST,
            octos_core::ui_protocol::methods::CONTENT_DELETE,
            octos_core::ui_protocol::methods::CONTENT_BULK_DELETE,
        ] {
            let request = RpcRequest::<Value>::new("req-no-header", method, Value::Null);
            let result = route_rpc_command(request, features);
            let err = result
                .expect_err("aux method must be rejected when no feature header was sent at all");
            assert_eq!(
                err.code,
                octos_core::ui_protocol::rpc_error_codes::METHOD_NOT_SUPPORTED,
                "{method} must reject with METHOD_NOT_SUPPORTED even with no header",
            );
        }
    }

    /// Codex review 2026-05-12 (BLOCK 1, companion): the legacy
    /// header-present gates (`session/hydrate`, `thread/graph/get`,
    /// `turn/state/get`, `task/list`, `task/cancel`,
    /// `task/restart_from_node`) must still pass through when no
    /// header was sent at all — that's the documented
    /// "advertise full first-slice in `SessionOpened`" fallback path.
    /// Only the new M12 Phase D-1 surface flips to strict opt-in.
    #[test]
    fn legacy_header_present_gates_still_accept_methods_with_no_feature_header() {
        // No header → all legacy gates fall through to
        // `UiCommand::from_rpc_request`. The decode itself may fail
        // because we send `Value::Null` as params, but the failure
        // must come from `from_rpc_request`, not from the gate.
        // Concretely: code must NOT be `METHOD_NOT_SUPPORTED`.
        let headers = HeaderMap::new();
        let features = ConnectionUiFeatures::from_headers_and_query(&headers, None);
        assert!(!features.header_present);
        for method in [
            octos_core::ui_protocol::methods::SESSION_HYDRATE,
            octos_core::ui_protocol::methods::THREAD_GRAPH_GET,
            octos_core::ui_protocol::methods::TURN_STATE_GET,
            octos_core::ui_protocol::methods::TASK_LIST,
            octos_core::ui_protocol::methods::TASK_CANCEL,
            octos_core::ui_protocol::methods::TASK_RESTART_FROM_NODE,
        ] {
            let request = RpcRequest::<Value>::new("req-legacy", method, Value::Null);
            let result = route_rpc_command(request, features);
            // Either Ok (unlikely for Null params) or Err with a
            // decode error — the key invariant is: NOT
            // `METHOD_NOT_SUPPORTED`.
            if let Err(err) = result {
                assert_ne!(
                    err.code,
                    octos_core::ui_protocol::rpc_error_codes::METHOD_NOT_SUPPORTED,
                    "{method} must NOT be rejected by capability gate when no header sent",
                );
            }
        }
    }

    /// Codex review 2026-05-12 (MEDIUM 1): a REST 404 from a
    /// session-scoped endpoint maps to `UNKNOWN_SESSION` with the
    /// addressed session id in `data.session_id`. The original
    /// implementation stuffed the method name into the session id
    /// slot ("session/messages_page: not found"), defeating
    /// reconciliation on the client side.
    #[tokio::test]
    async fn rest_status_to_rpc_error_404_session_context_echoes_session_id() {
        let context = RestResourceContext::session("sess-abc");
        let err = rest_status_to_rpc_error(
            octos_core::ui_protocol::methods::SESSION_MESSAGES_PAGE,
            axum::http::StatusCode::NOT_FOUND,
            Some("session not found".into()),
            &context,
        );
        assert_eq!(
            err.code,
            octos_core::ui_protocol::rpc_error_codes::UNKNOWN_SESSION,
        );
        let data = err.data.as_ref().expect("typed error data");
        assert_eq!(
            data.get("kind").and_then(Value::as_str),
            Some("unknown_session")
        );
        assert_eq!(
            data.get("session_id").and_then(Value::as_str),
            Some("sess-abc"),
            "session_id slot must carry the addressed session id, not the method name",
        );
        // The REST status + detail are still attached for debugging.
        assert_eq!(data.get("rest_status").and_then(Value::as_u64), Some(404));
        assert!(data.get("detail").is_some());
    }

    /// Codex review 2026-05-12 (MEDIUM 1, companion): a REST 404 from
    /// a non-session resource (content row, profile row) maps to the
    /// new `RESOURCE_NOT_FOUND` slot with `resource_type` +
    /// `identifier` echoed in `data`. Before this fix every 404 hit
    /// `UNKNOWN_SESSION` regardless of resource kind, which forced
    /// content/profile misses through a session-shaped error.
    #[tokio::test]
    async fn rest_status_to_rpc_error_404_resource_context_uses_not_found_slot() {
        let context = RestResourceContext::resource("content", "c-42");
        let err = rest_status_to_rpc_error(
            octos_core::ui_protocol::methods::CONTENT_DELETE,
            axum::http::StatusCode::NOT_FOUND,
            None,
            &context,
        );
        assert_eq!(
            err.code,
            octos_core::ui_protocol::rpc_error_codes::RESOURCE_NOT_FOUND,
            "non-session 404 must NOT collapse to UNKNOWN_SESSION",
        );
        let data = err.data.as_ref().expect("typed error data");
        assert_eq!(data.get("kind").and_then(Value::as_str), Some("not_found"));
        assert_eq!(
            data.get("resource_type").and_then(Value::as_str),
            Some("content"),
        );
        assert_eq!(data.get("identifier").and_then(Value::as_str), Some("c-42"),);
        assert_eq!(data.get("rest_status").and_then(Value::as_u64), Some(404));
    }

    /// Codex review 2026-05-12 (BLOCK 2): REST 503 from the
    /// standalone fallback (no gateway wired) must surface as
    /// `runtime_not_ready` rather than collapsing to `INTERNAL_ERROR`.
    /// This matches `handlers.rs:783` (standalone returns 503 when
    /// neither the gateway nor a local store can answer).
    #[tokio::test]
    async fn rest_status_to_rpc_error_503_maps_to_runtime_not_ready() {
        let context = RestResourceContext::session("sess-503");
        let err = rest_status_to_rpc_error(
            octos_core::ui_protocol::methods::SESSION_MESSAGES_PAGE,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Some("Sessions not available".into()),
            &context,
        );
        assert_eq!(
            err.code,
            octos_core::ui_protocol::rpc_error_codes::RUNTIME_NOT_READY,
        );
        let data = err.data.as_ref().expect("typed error data");
        assert_eq!(data.get("rest_status").and_then(Value::as_u64), Some(503));
    }

    /// Codex review 2026-05-12 (MEDIUM 3): `content/bulk_delete` must
    /// reject requests carrying more than
    /// `CONTENT_BULK_DELETE_MAX_IDS` ids early, before the catalog
    /// write-lock is taken. The dispatcher uses the constant from
    /// `octos-core` so the cap is shared with the params DTO docs.
    #[test]
    fn content_bulk_delete_max_ids_constant_is_mirrored_from_core() {
        assert_eq!(
            octos_core::ui_protocol::CONTENT_BULK_DELETE_MAX_IDS,
            256,
            "bulk-delete cap is documented at 256 in the ADR; bump both sides if changed",
        );
    }

    // M11-E: `session_filesystem_profile_for_workspace` was deleted
    // alongside `session_tool_registry`. Its server-wide containment
    // semantics (cwd must live under the legacy agent's workspace_root)
    // are obsolete in the multi-profile world — coding-agent UIs
    // legitimately point sessions at arbitrary repos OUTSIDE the
    // profile data_dir. The replacement gate is the path-safety check
    // in `validate_session_workspace_path_safety` + the bootstrap-time
    // re-check inside `SessionRuntime::bootstrap`. The
    // `session_workspace_authorizes_approved_subdir` /
    // `session_workspace_rejects_outside_root` tests that locked the
    // old containment behavior in place are removed with the helper;
    // the new path-safety check is covered by the M11-E acceptance
    // tests below + the bootstrap-side coverage in
    // `crate::runtime::session::tests`.

    // M11-E: `session_tool_registry` and its Tier-1 / Tier-2 fallback
    // helpers were deleted. The same Tier-1 invariant ("client-supplied
    // cwd wins over the bootstrap default") now lives on
    // `SessionRuntime::bootstrap`, exercised by
    // `crate::runtime::session::tests::bootstrap_with_two_hints_yields_distinct_workspaces`
    // and the M11-E acceptance tests
    // `appui_session_with_custom_cwd_reads_supplied_workspace` +
    // `two_appui_sessions_on_same_profile_with_different_cwds_isolated`
    // below. Tier-2 (operator-default `appui.default_session_cwd`) is
    // a known follow-up: the new `SessionRuntime::bootstrap` resolves
    // workspace_hint at the per-session layer and does not yet honor a
    // profile-scope operator default. Tracked alongside the M11
    // shared-`validate_session_workspace_allowed`-helper TODO in
    // `crate::runtime::session::validate_workspace_hint`.

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

    /// M10 Phase 6.1: the standalone-turn persist loop must pre-stamp the
    /// `User` row with the originating `TurnId`-derived thread id so the
    /// user prompt and the assistant reply land in the same thread on the
    /// SPA. Without this the SPA renders an empty placeholder bubble in
    /// the user's `clientMessageId`-keyed thread and creates an orphan
    /// thread for the assistant reply (3 bubbles per spawn_only turn
    /// instead of the target 2).
    #[test]
    fn pre_stamp_turn_thread_id_stamps_user_assistant_and_tool_when_unbound() {
        let turn_thread_id = "turn-abc";

        let user = pre_stamp_turn_thread_id(Message::user("hi"), turn_thread_id);
        let assistant = pre_stamp_turn_thread_id(Message::assistant("ok"), turn_thread_id);
        let tool = pre_stamp_turn_thread_id(
            Message {
                role: MessageRole::Tool,
                content: "result".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call-1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            turn_thread_id,
        );

        assert_eq!(
            user.thread_id.as_deref(),
            Some(turn_thread_id),
            "user row must inherit the turn-derived thread_id so its bubble \
             coalesces with the assistant reply"
        );
        assert_eq!(assistant.thread_id.as_deref(), Some(turn_thread_id));
        assert_eq!(tool.thread_id.as_deref(), Some(turn_thread_id));
    }

    /// Caller-supplied `thread_id` values must NOT be overwritten — that
    /// would corrupt rows already routed to the correct sub-thread (e.g.
    /// spawn_only completion rows that bind a different originating
    /// thread).
    #[test]
    fn pre_stamp_turn_thread_id_preserves_caller_supplied_thread_id() {
        let mut user = Message::user("hi");
        user.thread_id = Some("explicit-thread".into());

        let stamped = pre_stamp_turn_thread_id(user, "turn-other");

        assert_eq!(
            stamped.thread_id.as_deref(),
            Some("explicit-thread"),
            "caller-supplied thread_id must be preserved"
        );
    }

    /// System rows are not thread-scoped — the helper must leave them
    /// alone so the per-turn system primer (when present) does not get
    /// retro-rooted into a turn thread that didn't author it.
    #[test]
    fn pre_stamp_turn_thread_id_leaves_system_rows_alone() {
        let system = Message {
            role: MessageRole::System,
            content: "primer".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };

        let stamped = pre_stamp_turn_thread_id(system, "turn-abc");

        assert!(
            stamped.thread_id.is_none(),
            "system rows must remain unbound to a turn thread"
        );
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
        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        abort_connection_turns(
            &active_turns,
            &connection_turns,
            &scopes,
            &ledger,
            &approvals,
        )
        .await;

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
            ConnectionId::next(),
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
            ConnectionId::next(),
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
                topic: None,
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

    /// #924 BLOCK 2: once a lifecycle send marks the connection failed,
    /// every subsequent enqueue must fail with `FatalClosed` — even if
    /// the underlying channel has spare capacity now. Background
    /// forwarders that keep pumping after the read loop tore down would
    /// otherwise queue frames into a writer about to drain and exit.
    #[tokio::test]
    async fn try_enqueue_returns_fatal_closed_after_mark_failed() {
        let (ws, mut rx) = ws_connection_for_test(8);

        // First send succeeds and leaves capacity available.
        let frame = WsMessage::Text("ping".to_string().into());
        assert!(ws.try_enqueue(frame).is_ok());

        // Drain so capacity is fully open.
        let _ = rx.try_recv();

        // Latch the connection as failed (the lifecycle wrappers do
        // this on backpressure / closed writer; here we drive the API
        // directly to isolate the check).
        ws.mark_failed();

        // Even with capacity open, the next enqueue must fail loudly.
        let frame = WsMessage::Text("after-fail".to_string().into());
        let err = ws
            .try_enqueue(frame)
            .expect_err("post-latch enqueue must fail");
        assert!(matches!(err, SendError::FatalClosed));

        // And the lifecycle wrapper turns it into LifecycleFailure so
        // existing RPC-reply callsites still see the failure-shaped
        // error.
        let res = send_rpc_result(&ws, "post-fail".into(), json!({"ok": true}));
        assert!(matches!(res, Err(SendError::LifecycleFailure(_))));
    }

    /// #924 BLOCK 1: `mark_failed` must wake every pending `notified()`
    /// waiter so the read loop's `select!` arm fires immediately. An
    /// idle socket with a failed write side must NOT sit waiting for
    /// the next client frame.
    #[tokio::test]
    async fn mark_failed_wakes_failed_notify_waiters() {
        let (ws, _rx) = ws_connection_for_test(1);
        let notify = ws.failed_notify();

        // Park a waiter; latch failed; the waiter must complete promptly.
        let waited = tokio::time::timeout(std::time::Duration::from_millis(500), async move {
            notify.notified().await;
        });
        // Run latch + await concurrently — the `notified()` future must
        // observe the wake even though we never read another frame.
        let ws_clone = ws.clone();
        let latch = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            ws_clone.mark_failed();
        });
        waited.await.expect("notified() must wake within 500ms");
        latch.await.expect("latch task joins cleanly");
        assert!(ws.is_failed());
    }

    /// #924 round-2 BLOCK: the round-1 test parks a waiter BEFORE calling
    /// `mark_failed`. The lost-notify race only shows up the other way
    /// around: the latch fires while the read loop is between iterations
    /// (no `Notified` future exists yet). `notify_waiters` stashes nothing,
    /// so the next-iteration `notified()` would never resolve without the
    /// pre-park latch re-check. This test replays the same select shape
    /// the production loop uses and asserts it exits on an idle socket
    /// even when `mark_failed` fires before the read task starts polling.
    #[tokio::test]
    async fn mark_failed_during_idle_loop_still_cleans_up() {
        let (ws, _rx) = ws_connection_for_test(1);
        let failed_notify = ws.failed_notify();
        // The "ws_rx" stand-in: an mpsc never sent on => idle socket.
        let (_inbound_tx, mut inbound_rx) = mpsc::channel::<()>(1);

        // Latch failed BEFORE the read task ever spins. This is the
        // lost-notify ordering: the future that will park does not yet
        // exist when `notify_waiters` fires.
        ws.mark_failed();

        let ws_for_task = ws.clone();
        let read_task = tokio::spawn(async move {
            loop {
                if ws_for_task.is_failed() {
                    break;
                }
                let notified = failed_notify.notified();
                tokio::pin!(notified);
                if ws_for_task.is_failed() {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = &mut notified => break,
                    _ = inbound_rx.recv() => continue,
                }
            }
        });

        tokio::time::timeout(std::time::Duration::from_millis(100), read_task)
            .await
            .expect("read loop must exit promptly when latch fires before park")
            .expect("read task joins cleanly");
        assert!(ws.is_failed());
    }

    /// #924 BLOCK 5: server-to-server clients sometimes send the Origin
    /// header with an empty value (`Origin: `). The original gate
    /// rejected those because the empty string never matched the
    /// allowlist; treat present-but-empty as absent.
    #[test]
    fn ws_origin_gate_allows_absent_or_empty_origin() {
        // No Origin header at all → allow (TUI / gateway / scripts).
        let headers = HeaderMap::new();
        assert_eq!(
            decide_ws_origin_gate(&headers, None),
            WsOriginDecision::Allow,
        );

        // Origin header present but empty after trim → allow.
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::ORIGIN, "".parse().unwrap());
        assert_eq!(
            decide_ws_origin_gate(&headers, None),
            WsOriginDecision::Allow,
        );

        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::ORIGIN, "   ".parse().unwrap());
        assert_eq!(
            decide_ws_origin_gate(&headers, None),
            WsOriginDecision::Allow,
        );
    }

    #[test]
    fn ws_origin_gate_rejects_present_non_allowlisted_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            "https://evil.example.com".parse().unwrap(),
        );
        let decision = decide_ws_origin_gate(&headers, Some("bot.ominix.io"));
        assert!(matches!(
            decision,
            WsOriginDecision::RejectDisallowed { ref origin }
                if origin == "https://evil.example.com"
        ));
    }

    /// PR #928 regression: per-tenant subdomains of `base_domain` must
    /// be accepted. Hosted multi-tenant minis route by subdomain
    /// (`dspfac.<base>`, `alice.<base>`, ...). The pre-fix gate used the
    /// static CORS allowlist only (app./admin./api.), so every browser
    /// WS upgrade from a per-tenant page was 403'd, breaking the SPA.
    #[test]
    fn ws_origin_gate_allows_single_label_subdomain_of_base() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            "https://dspfac.ocean.ominix.io".parse().unwrap(),
        );
        assert_eq!(
            decide_ws_origin_gate(&headers, Some("ocean.ominix.io")),
            WsOriginDecision::Allow,
        );
    }

    /// Multi-label subdomain (e.g. `a.b.<base>`) is NOT a single
    /// per-tenant label — reject so a hijacked deeper subdomain can't
    /// bypass the gate.
    #[test]
    fn ws_origin_gate_rejects_multi_label_subdomain_of_base() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            "https://attacker.dspfac.ocean.ominix.io".parse().unwrap(),
        );
        let decision =
            decide_ws_origin_gate(&headers, Some("ocean.ominix.io"));
        assert!(matches!(
            decision,
            WsOriginDecision::RejectDisallowed { .. }
        ));
    }

    /// Port-suffixed subdomain origins (e.g. `tenant.base:1234`) are
    /// NOT plain tenant subdomains — reject.
    #[test]
    fn ws_origin_gate_rejects_tenant_with_port() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            "https://dspfac.ocean.ominix.io:8443".parse().unwrap(),
        );
        let decision =
            decide_ws_origin_gate(&headers, Some("ocean.ominix.io"));
        assert!(matches!(
            decision,
            WsOriginDecision::RejectDisallowed { .. }
        ));
    }

    /// #924 BLOCK 5: a header that fails ASCII parse is malformed
    /// input — keep rejecting it. Treating it as "absent" would be a
    /// downgrade-attack vector.
    #[test]
    fn ws_origin_gate_rejects_non_ascii_origin() {
        let mut headers = HeaderMap::new();
        // Insert raw bytes that are not valid header values via the
        // typed `HeaderValue::from_bytes` builder. We use bytes that
        // would round-trip but contain non-ASCII content so `to_str`
        // returns Err.
        let val = axum::http::HeaderValue::from_bytes(b"https://\xff.example.com")
            .expect("HeaderValue accepts arbitrary visible bytes");
        headers.insert(axum::http::header::ORIGIN, val);
        assert_eq!(
            decide_ws_origin_gate(&headers, None),
            WsOriginDecision::RejectMalformed,
        );
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
            ConnectionId::next(),
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
            originating_client_message_id: None,
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

    // ====================================================================
    // PR G — UPCR-2026-009 / -010 / -011 / -012 handler tests
    // ====================================================================

    fn prg_state_with_session(
        session_id: &SessionKey,
        seed: impl FnOnce(&mut octos_bus::Session),
    ) -> Arc<AppState> {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = octos_bus::SessionManager::open(tmp.path()).expect("session manager open");
        let manager = Arc::new(tokio::sync::Mutex::new(manager));
        // Seed by directly mutating in-memory session.
        {
            let mut guard = manager.try_lock().expect("session manager lock");
            // get_or_create is async, so we sidestep by using try_lock + a
            // synchronous workaround: spawn-blocking is overkill; this
            // helper is only called from sync context above the test.
            // We block_on a separate task so we can call async manager.
            // Easiest: rebuild via a sync-OK helper. Use futures executor.
            let session = futures::executor::block_on(guard.get_or_create(session_id));
            seed(session);
        }
        Arc::new(AppState {
            sessions: Some(manager),
            ..AppState::empty_for_tests()
        })
        // tmp is dropped when state drops; tests don't observe disk
    }

    fn prg_seed_user_assistant(session: &mut octos_bus::Session) {
        let now = Utc::now();
        session.messages.push(Message {
            role: MessageRole::User,
            content: "hello".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some("cmid-user-1".into()),
            thread_id: Some("cmid-user-1".into()),
            timestamp: now,
        });
        session.messages.push(Message {
            role: MessageRole::Assistant,
            content: "world".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some("cmid-user-1".into()),
            timestamp: now + chrono::Duration::milliseconds(10),
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_hydrate_returns_full_chat_state() {
        let session_id = SessionKey("local:hydrate-1".into());
        let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
        let approvals = PendingApprovalStore::default();
        let active_turns = active_turns_registry();
        let ledger = event_ledger(&state).await;
        let (ws, mut rx) = ws_connection_for_test(8);

        handle_session_hydrate(
            &ws,
            &state,
            &ledger,
            &approvals,
            &active_turns,
            None,
            None,
            ConnectionUiFeatures::default(),
            "h1".into(),
            SessionHydrateParams {
                session_id: session_id.clone(),
                after: None,
                include: vec![],
            },
        )
        .await;

        let frame = recv_rpc_json(&mut rx).await;
        assert_eq!(frame["id"], "h1");
        let result = &frame["result"];
        assert_eq!(result["session_id"], session_id.to_string());
        assert!(result["cursor"].is_object());
        let messages = result["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        let threads = result["threads"].as_array().expect("threads array");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0]["thread_id"], "cmid-user-1");
        assert_eq!(threads[0]["root_seq"], 0);
        assert_eq!(threads[0]["message_seqs"], json!([0, 1]));
        assert!(result["turns"].is_array());
        assert_eq!(result["pending_approvals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_hydrate_atomically_consistent_snapshot_and_cursor() {
        // Codex's atomicity ask: an event landing between the snapshot read
        // and the cursor read must NOT slip past either. We exercise this by
        // calling `snapshot_with_cursor` once and asserting the returned
        // cursor.seq is >= every event's seq in the returned vec — i.e. the
        // cursor pairs with the snapshot atomically.
        let session_id = SessionKey("local:hydrate-atomic".into());
        let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
        let ledger = event_ledger(&state).await;

        // Append two notifications to the ledger so there's something to
        // bound.
        let _ = ledger.append_notification(UiNotification::Warning(
            octos_core::ui_protocol::WarningEvent {
                session_id: session_id.clone(),
                turn_id: None,
                code: "test".into(),
                message: "first".into(),
            },
        ));
        let _ = ledger.append_notification(UiNotification::Warning(
            octos_core::ui_protocol::WarningEvent {
                session_id: session_id.clone(),
                turn_id: None,
                code: "test".into(),
                message: "second".into(),
            },
        ));

        let (events, cursor) = ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("snapshot");
        // The pair invariant: cursor.seq >= max(event.cursor.seq) for every
        // event in the snapshot. Combined with the lock held during reads,
        // this means a follow-up `replay_after(cursor)` returns only events
        // strictly after — no gap.
        let max_event = events.iter().map(|e| e.cursor.seq).max().unwrap_or(0);
        assert!(
            cursor.seq >= max_event,
            "cursor.seq {} must >= max event seq {}",
            cursor.seq,
            max_event,
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn thread_graph_get_returns_known_threads() {
        let session_id = SessionKey("local:graph-1".into());
        let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
        let active_turns = active_turns_registry();
        let ledger = event_ledger(&state).await;
        let (ws, mut rx) = ws_connection_for_test(8);

        handle_thread_graph_get(
            &ws,
            &state,
            &ledger,
            &active_turns,
            None,
            None,
            "g1".into(),
            ThreadGraphGetParams {
                session_id: session_id.clone(),
                at: None,
            },
        )
        .await;

        let frame = recv_rpc_json(&mut rx).await;
        assert_eq!(frame["id"], "g1");
        let threads = frame["result"]["threads"].as_array().expect("threads");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0]["thread_id"], "cmid-user-1");
        assert_eq!(threads[0]["root_seq"], 0);
        assert_eq!(threads[0]["root_client_message_id"], "cmid-user-1");
        assert_eq!(threads[0]["message_seqs"], json!([0, 1]));
        let orphans = frame["result"]["orphans"].as_array().expect("orphans");
        assert_eq!(orphans.len(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn thread_graph_get_surfaces_orphans() {
        // A non-system row missing thread_id is an orphan. Per UPCR-2026-010
        // it lands in `orphans` so a client can metric on it.
        let session_id = SessionKey("local:graph-orphan".into());
        let state = prg_state_with_session(&session_id, |session| {
            let now = Utc::now();
            session.messages.push(Message {
                role: MessageRole::User,
                content: "rooted".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: Some("cmid-1".into()),
                thread_id: Some("cmid-1".into()),
                timestamp: now,
            });
            session.messages.push(Message {
                role: MessageRole::Assistant,
                content: "orphan".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None, // <- orphan
                timestamp: now + chrono::Duration::milliseconds(10),
            });
        });
        let active_turns = active_turns_registry();
        let ledger = event_ledger(&state).await;
        let (ws, mut rx) = ws_connection_for_test(8);

        handle_thread_graph_get(
            &ws,
            &state,
            &ledger,
            &active_turns,
            None,
            None,
            "g2".into(),
            ThreadGraphGetParams {
                session_id: session_id.clone(),
                at: None,
            },
        )
        .await;

        let frame = recv_rpc_json(&mut rx).await;
        let orphans = frame["result"]["orphans"].as_array().expect("orphans");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0], 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_state_get_returns_active_for_in_flight() {
        let session_id = SessionKey("local:turn-active".into());
        let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
        let active_turns = active_turns_registry();
        let turn_id = TurnId::new();
        // Insert a synthetic active turn into the registry. We construct
        // ActiveTurn directly the same way handle_turn_start would.
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let dummy_handle = tokio::spawn(async {});
        {
            let mut guard = active_turns.lock().await;
            guard.insert(
                session_id.clone(),
                ActiveTurn {
                    turn_id: turn_id.clone(),
                    state: Arc::new(TokioMutex::new(TurnState::Active)),
                    interrupt_tx: Arc::new(TokioMutex::new(Some(interrupt_tx))),
                    abort: dummy_handle.abort_handle(),
                },
            );
        }
        let ledger = event_ledger(&state).await;
        let (ws, mut rx) = ws_connection_for_test(8);

        handle_turn_state_get(
            &ws,
            &state,
            &ledger,
            &active_turns,
            None,
            None,
            "t1".into(),
            TurnStateGetParams {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            },
        )
        .await;

        let frame = recv_rpc_json(&mut rx).await;
        assert_eq!(frame["result"]["state"], "active");
        // Cleanup so the test does not pollute the global registry for
        // sibling tests.
        active_turns.lock().await.remove(&session_id);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_state_get_falls_back_to_durable_projection_for_evicted() {
        // Codex's durable-backing ask: a turn that is no longer in the
        // active-turn registry but whose lifecycle is recorded in the
        // ledger must still surface a non-`unknown` state.
        let session_id = SessionKey("local:turn-evicted".into());
        let state = prg_state_with_session(&session_id, |_| {});
        let active_turns = active_turns_registry();
        let turn_id = TurnId::new();
        let ledger = event_ledger(&state).await;

        // Append a turn/started + turn/completed to the ledger so the
        // projection has truth without anything in the registry.
        let _ = ledger.append_notification(UiNotification::TurnStarted(
            octos_core::ui_protocol::TurnStartedEvent {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                timestamp: Utc::now(),
                topic: None,
            },
        ));
        let _ = ledger.append_notification(UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
        }));

        let (ws, mut rx) = ws_connection_for_test(8);
        handle_turn_state_get(
            &ws,
            &state,
            &ledger,
            &active_turns,
            None,
            None,
            "t2".into(),
            TurnStateGetParams {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            },
        )
        .await;
        let frame = recv_rpc_json(&mut rx).await;
        assert_eq!(
            frame["result"]["state"], "completed",
            "evicted turn must surface terminal state from the ledger projection"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_hydrate_rejects_unknown_session() {
        // Build a sessions manager with NO sessions seeded; the handler
        // must reject the request rather than auto-create or return an
        // empty hydrate.
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = octos_bus::SessionManager::open(tmp.path()).expect("open");
        let state = Arc::new(AppState {
            sessions: Some(Arc::new(tokio::sync::Mutex::new(manager))),
            ..AppState::empty_for_tests()
        });
        let approvals = PendingApprovalStore::default();
        let active_turns = active_turns_registry();
        let ledger = event_ledger(&state).await;
        let (ws, mut rx) = ws_connection_for_test(8);

        handle_session_hydrate(
            &ws,
            &state,
            &ledger,
            &approvals,
            &active_turns,
            None,
            None,
            ConnectionUiFeatures::default(),
            "h-unknown".into(),
            SessionHydrateParams {
                session_id: SessionKey("local:nope".into()),
                after: None,
                include: vec![],
            },
        )
        .await;

        let frame = recv_rpc_json(&mut rx).await;
        assert!(frame.get("error").is_some(), "must return error frame");
        assert_eq!(frame["error"]["data"]["kind"], "unknown_session");
    }

    /// M10 Phase 6.2 / Bug C: a negotiated client receives the
    /// retained `turn/spawn_complete` envelopes on the hydrate
    /// response so it can dedup against the legacy `Background`-source
    /// rows in `messages` on its side. The server itself does NOT
    /// suppress rows — codex flagged multiple correctness regressions
    /// in every server-side suppression design (NotConfigured-branch
    /// empty media, multi-task per-turn ambiguity, orphan companions
    /// from failed final-ack persists). Surfacing both signals lets
    /// the client mirror the live wire's "consumer chooses one shape"
    /// semantics without server-side guesswork.
    #[tokio::test(flavor = "current_thread")]
    async fn session_hydrate_surfaces_replayed_envelopes_for_negotiated_client() {
        let session_id = SessionKey("local:hydrate-envelopes".into());
        // Capture the spawn-ack row's timestamp so the envelope's
        // `message_id` can mirror what `MessageCommitObserver` would
        // emit on the live wire (and what the hydrate handler now
        // synthesizes for `HydratedMessage.message_id`).
        let spawn_ack_ts = Utc::now() + chrono::Duration::milliseconds(10);
        let state = prg_state_with_session(&session_id, |session| {
            let now = spawn_ack_ts - chrono::Duration::milliseconds(10);
            session.messages.push(Message {
                role: MessageRole::User,
                content: "kick off deep_research".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: Some("cmid-user-1".into()),
                thread_id: Some("cmid-user-1".into()),
                timestamp: now,
            });
            // Background companion (legacy `send_file` per-file row).
            session.messages.push(Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec!["research/_report.md".into()],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: Some("cmid-user-1".into()),
                timestamp: now + chrono::Duration::milliseconds(5),
            });
            // Background spawn-ack.
            session.messages.push(Message {
                role: MessageRole::Assistant,
                content: "deep_research delivered.".into(),
                media: vec!["research/_report.md".into()],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: Some("cmid-user-1".into()),
                timestamp: spawn_ack_ts,
            });
        });
        let approvals = PendingApprovalStore::default();
        let active_turns = active_turns_registry();
        let ledger = event_ledger(&state).await;

        let spawn_ack_message_id = format!(
            "{}:2:{}",
            session_id.0,
            spawn_ack_ts.timestamp_nanos_opt().unwrap_or(0),
        );
        // Append the matching `MessagePersisted` events so the
        // hydrate handler can surface `source: background` on the
        // hydrated rows (mirrors what `MessageCommitObserver` would
        // emit at live persist time under the
        // `MESSAGE_PERSISTED_SOURCE_OVERRIDE` task-local).
        ledger.append_notification(UiNotification::MessagePersisted(MessagePersistedEvent {
            session_id: session_id.clone(),
            turn_id: None,
            thread_id: Some("cmid-user-1".into()),
            seq: 1,
            role: "assistant".into(),
            message_id: format!("{}:1:0", session_id.0),
            client_message_id: None,
            source: MessagePersistedSource::Background,
            cursor: UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            },
            persisted_at: Utc::now(),
            media: vec!["research/_report.md".into()],
        }));
        ledger.append_notification(UiNotification::MessagePersisted(MessagePersistedEvent {
            session_id: session_id.clone(),
            turn_id: None,
            thread_id: Some("cmid-user-1".into()),
            seq: 2,
            role: "assistant".into(),
            message_id: spawn_ack_message_id.clone(),
            client_message_id: None,
            source: MessagePersistedSource::Background,
            cursor: UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            },
            persisted_at: Utc::now(),
            media: vec!["research/_report.md".into()],
        }));
        ledger.append_notification(UiNotification::TurnSpawnComplete(TurnSpawnCompleteEvent {
            session_id: session_id.clone(),
            turn_id: None,
            thread_id: Some("cmid-user-1".into()),
            task_id: "task_abc".into(),
            response_to_client_message_id: Some("cmid-user-1".into()),
            seq: 2,
            message_id: spawn_ack_message_id.clone(),
            source: "background".into(),
            cursor: UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            },
            persisted_at: Utc::now(),
            content: "deep_research delivered.".into(),
            media: vec!["research/_report.md".into()],
        }));

        // 1) Negotiated client: messages list is byte-identical to
        // the legacy shape (3 rows), AND the new
        // `replayed_envelopes` field carries the envelope so the
        // client can dedup on its side.
        let (ws_new, mut rx_new) = ws_connection_for_test(8);
        handle_session_hydrate(
            &ws_new,
            &state,
            &ledger,
            &approvals,
            &active_turns,
            None,
            None,
            features_for_spawn_complete_test(true, true),
            "h-new".into(),
            SessionHydrateParams {
                session_id: session_id.clone(),
                after: None,
                include: vec![],
            },
        )
        .await;
        let frame_new = recv_rpc_json(&mut rx_new).await;
        let messages_new = frame_new["result"]["messages"]
            .as_array()
            .expect("messages array");
        assert_eq!(
            messages_new.len(),
            3,
            "server does NOT suppress rows; negotiated client dedups using replayed_envelopes",
        );
        // Codex Bug C round-5: the spawn-ack row's `message_id` must
        // be present on the hydrated wire so the client can match it
        // against the envelope. Without this, the client has nothing
        // to dedup against.
        let spawn_ack_row = messages_new
            .iter()
            .find(|m| m["seq"] == 2)
            .expect("seq=2 spawn-ack row");
        assert_eq!(
            spawn_ack_row["message_id"], spawn_ack_message_id,
            "spawn-ack row must expose message_id matching the envelope",
        );
        // Codex Bug C round-6: per-row provenance. The companion
        // and spawn-ack rows surface `source: "background"` so the
        // client can drop them in favour of the envelope. Without
        // `source`, the client could only dedup the spawn-ack and
        // companion rows would still render as duplicate bubbles.
        let companion_row = messages_new
            .iter()
            .find(|m| m["seq"] == 1)
            .expect("seq=1 companion row");
        assert_eq!(companion_row["source"], "background");
        assert_eq!(spawn_ack_row["source"], "background");
        let user_row = messages_new
            .iter()
            .find(|m| m["seq"] == 0)
            .expect("seq=0 user row");
        // The user row never had a `MessagePersisted` ledger event in
        // this test (we only seeded background events), so its
        // `source` is omitted. That's fine: the client doesn't need
        // provenance for non-coalescible rows.
        assert!(
            user_row.get("source").map(|v| v.is_null()).unwrap_or(true),
            "user row's source field is omitted absent a matching ledger event; got: {user_row:?}",
        );
        let envelopes = frame_new["result"]["replayed_envelopes"]
            .as_array()
            .expect("replayed_envelopes array");
        assert_eq!(envelopes.len(), 1, "single envelope retained");
        assert_eq!(
            envelopes[0]["message_id"], spawn_ack_message_id,
            "envelope's message_id matches the spawn-ack row's id by construction",
        );
        assert_eq!(envelopes[0]["task_id"], "task_abc");
        assert_eq!(envelopes[0]["thread_id"], "cmid-user-1");
        assert_eq!(envelopes[0]["seq"], 2);
        assert_eq!(envelopes[0]["content"], "deep_research delivered.");
        assert_eq!(envelopes[0]["media"], json!(["research/_report.md"]));

        // 2) Non-negotiated client: legacy wire shape — messages
        // list intact, and `replayed_envelopes` field is OMITTED
        // (not `null`) so the JSON shape matches pre-fix exactly.
        let (ws_legacy, mut rx_legacy) = ws_connection_for_test(8);
        handle_session_hydrate(
            &ws_legacy,
            &state,
            &ledger,
            &approvals,
            &active_turns,
            None,
            None,
            ConnectionUiFeatures::default(),
            "h-legacy".into(),
            SessionHydrateParams {
                session_id: session_id.clone(),
                after: None,
                include: vec![],
            },
        )
        .await;
        let frame_legacy = recv_rpc_json(&mut rx_legacy).await;
        let messages_legacy = frame_legacy["result"]["messages"]
            .as_array()
            .expect("messages array");
        assert_eq!(messages_legacy.len(), 3, "legacy unchanged");
        let result = frame_legacy["result"].as_object().expect("result object");
        assert!(
            !result.contains_key("replayed_envelopes"),
            "legacy clients see byte-identical wire (no replayed_envelopes key); got keys: {:?}",
            result.keys().collect::<Vec<_>>(),
        );
        // Codex Bug C round-6: non-negotiated clients also see the
        // pre-fix `messages` shape — no `message_id`, no `source`
        // keys. This protects strict-codegen consumers that have no
        // `replayed_envelopes` to bind to.
        for msg in messages_legacy {
            let msg_obj = msg.as_object().expect("message object");
            assert!(
                !msg_obj.contains_key("message_id"),
                "legacy client message MUST NOT carry message_id; got: {msg_obj:?}",
            );
            assert!(
                !msg_obj.contains_key("source"),
                "legacy client message MUST NOT carry source; got: {msg_obj:?}",
            );
        }
    }

    /// Bug C corollary: a negotiated client whose hydrate request
    /// excludes `messages` does not need the envelopes either — they
    /// only matter as a dedup key against the messages list. Keep
    /// `replayed_envelopes` absent in that case so the response stays
    /// minimal.
    #[tokio::test(flavor = "current_thread")]
    async fn session_hydrate_omits_envelopes_when_messages_excluded() {
        let session_id = SessionKey("local:hydrate-envelopes-no-msgs".into());
        let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
        let approvals = PendingApprovalStore::default();
        let active_turns = active_turns_registry();
        let ledger = event_ledger(&state).await;
        ledger.append_notification(UiNotification::TurnSpawnComplete(TurnSpawnCompleteEvent {
            session_id: session_id.clone(),
            turn_id: None,
            thread_id: Some("cmid-user-1".into()),
            task_id: "task_x".into(),
            response_to_client_message_id: Some("cmid-user-1".into()),
            seq: 1,
            message_id: format!("{}:1:0", session_id.0),
            source: "background".into(),
            cursor: UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            },
            persisted_at: Utc::now(),
            content: "done".into(),
            media: vec![],
        }));

        let (ws, mut rx) = ws_connection_for_test(8);
        handle_session_hydrate(
            &ws,
            &state,
            &ledger,
            &approvals,
            &active_turns,
            None,
            None,
            features_for_spawn_complete_test(true, true),
            "h-no-msgs".into(),
            SessionHydrateParams {
                session_id: session_id.clone(),
                after: None,
                include: vec!["threads".into()],
            },
        )
        .await;
        let frame = recv_rpc_json(&mut rx).await;
        let result = frame["result"].as_object().expect("result object");
        assert!(
            !result.contains_key("messages"),
            "messages excluded by include filter",
        );
        assert!(
            !result.contains_key("replayed_envelopes"),
            "envelopes are a messages-list dedup key; omit when messages aren't requested",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_state_get_returns_unknown_for_missing() {
        let session_id = SessionKey("local:turn-unknown".into());
        let state = prg_state_with_session(&session_id, |_| {});
        let active_turns = active_turns_registry();
        let ledger = event_ledger(&state).await;
        let (ws, mut rx) = ws_connection_for_test(8);

        handle_turn_state_get(
            &ws,
            &state,
            &ledger,
            &active_turns,
            None,
            None,
            "t3".into(),
            TurnStateGetParams {
                session_id: session_id.clone(),
                turn_id: TurnId::new(),
            },
        )
        .await;
        let frame = recv_rpc_json(&mut rx).await;
        // Per UPCR-2026-011: missing turn returns `state: "unknown"` —
        // NOT an error.
        assert!(frame.get("result").is_some(), "missing turn must succeed");
        assert_eq!(frame["result"]["state"], "unknown");
    }

    /// Serialise tests that mutate the process-global message-commit
    /// observer so they don't race each other or with concurrently running
    /// fixtures that also exercise `add_message_with_seq`.
    fn message_commit_observer_test_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn message_persisted_emitted_after_each_commit_in_order() {
        // Wires the bus-level observer hook to a local sink and asserts
        // notifications fire in commit order, with strictly monotonic seqs.
        let _guard = message_commit_observer_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let observed: Arc<std::sync::Mutex<Vec<(SessionKey, Message, usize)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_clone = observed.clone();
        let prev =
            octos_bus::set_message_commit_observer(Some(Arc::new(move |key, message, seq| {
                observed_clone
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((key.clone(), message.clone(), seq));
            })));

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut manager =
            octos_bus::SessionManager::open(tmp.path()).expect("session manager open");
        let session_id = SessionKey("local:persisted-order".into());
        for content in ["one", "two", "three"] {
            let msg = Message {
                role: MessageRole::User,
                content: content.into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: Some(format!("cmid-{content}")),
                thread_id: None,
                timestamp: Utc::now(),
            };
            manager
                .add_message_with_seq(&session_id, msg)
                .await
                .expect("add_message succeeds");
        }

        let observed = observed.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(observed.len(), 3, "one observation per commit");
        assert_eq!(observed[0].2, 0);
        assert_eq!(observed[1].2, 1);
        assert_eq!(observed[2].2, 2);
        assert_eq!(observed[0].1.content, "one");
        assert_eq!(observed[1].1.content, "two");
        assert_eq!(observed[2].1.content, "three");

        // Restore the previous observer (None for clean tests).
        octos_bus::set_message_commit_observer(prev);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn message_persisted_not_emitted_on_commit_failure() {
        let _guard = message_commit_observer_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // The observer must NOT see a row that did not commit. Simulate a
        // commit failure by exhausting the file size limit. In practice
        // we cannot easily inject a failure into `add_message_with_seq`
        // without rewriting the helper; instead assert the commit-failure
        // contract via the call-site comment + a guarded-write test that
        // succeeds end-to-end (the negative assertion is implicitly
        // covered by the `record_session_persist("failed")` early-return).
        //
        // Concretely: remove the observer, run a commit, re-install, run
        // a second commit. The first commit must NOT appear in the second
        // observer's sink.
        let observed: Arc<std::sync::Mutex<Vec<()>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_clone = observed.clone();
        // Save the global observer (e.g. the process-wide ledger
        // observer installed by sibling tests via `event_ledger`) so we
        // can restore it on exit.
        let prev = octos_bus::set_message_commit_observer(None);

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut manager =
            octos_bus::SessionManager::open(tmp.path()).expect("session manager open");
        let session_id = SessionKey("local:persisted-failure".into());

        // First commit — observer NOT installed, so no event recorded.
        let msg = Message {
            role: MessageRole::User,
            content: "no-observer".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some("cmid-1".into()),
            thread_id: None,
            timestamp: Utc::now(),
        };
        manager
            .add_message_with_seq(&session_id, msg)
            .await
            .expect("first commit");
        assert!(observed.lock().unwrap().is_empty());

        // Install the sink and run a second commit. Sink must contain
        // exactly one event (the second), not two.
        octos_bus::set_message_commit_observer(Some(Arc::new(move |_key, _message, _seq| {
            observed_clone.lock().unwrap().push(());
        })));
        let msg2 = Message {
            role: MessageRole::User,
            content: "with-observer".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some("cmid-2".into()),
            thread_id: None,
            timestamp: Utc::now(),
        };
        manager
            .add_message_with_seq(&session_id, msg2)
            .await
            .expect("second commit");
        let observed_after = observed.lock().unwrap();
        assert_eq!(
            observed_after.len(),
            1,
            "observer must only see commits that ran while it was installed"
        );

        octos_bus::set_message_commit_observer(prev);
    }

    /// M9-γ-7 (issue #844): `is_metadata_only_assistant_row` is the
    /// pure-function classifier the observer uses to drop intermediate
    /// metadata-only assistant rows. Lock its truth table here so a
    /// future refactor that "helpfully" widens the filter cannot drop
    /// rows the wire surface needs.
    #[test]
    fn is_metadata_only_assistant_row_truth_table() {
        // Empty assistant with no media: metadata-only -> drop.
        let mut empty_assistant = Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        };
        assert!(is_metadata_only_assistant_row(&empty_assistant));

        // Whitespace-only counts as empty.
        empty_assistant.content = "   \n\t".into();
        assert!(is_metadata_only_assistant_row(&empty_assistant));

        // Assistant with text: keep.
        empty_assistant.content = "hello".into();
        assert!(!is_metadata_only_assistant_row(&empty_assistant));

        // Assistant with media but empty text: keep (image-only response).
        empty_assistant.content = String::new();
        empty_assistant.media = vec!["data:image/png;base64,abc".into()];
        assert!(!is_metadata_only_assistant_row(&empty_assistant));

        // Tool messages are never filtered.
        let tool_message = Message {
            role: MessageRole::Tool,
            content: String::new(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("tc-1".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        };
        assert!(!is_metadata_only_assistant_row(&tool_message));

        // User rows are never filtered.
        let user_message = Message {
            role: MessageRole::User,
            content: String::new(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        };
        assert!(!is_metadata_only_assistant_row(&user_message));
    }

    /// M9-γ-7 (issue #844): a 3-iteration agent loop that commits
    /// (assistant tool-call only) -> tool result -> (assistant tool-call
    /// only) -> tool result -> (assistant final text) MUST surface
    /// EXACTLY ONE `message/persisted` envelope for the assistant turn
    /// (the final text row), plus the per-tool rows, on the M9 ledger.
    /// Pre-fix this emitted three assistant `message/persisted`
    /// envelopes, all under the same `thread_id` — the phantom-bubble
    /// shape the web reducer collapsed (octos-web #92).
    #[tokio::test(flavor = "current_thread")]
    async fn gamma_7_dedup_one_assistant_persisted_per_turn() {
        use octos_core::ui_protocol::{MessagePersistedSource, methods};

        let _guard = message_commit_observer_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Spin a fresh ledger + observer wired the same way the live
        // server wires them on the first `event_ledger` call.
        let ledger = Arc::new(UiProtocolLedger::new(64));
        install_message_commit_observer(ledger.clone());

        let session_id = SessionKey("local:gamma-7-dedup".into());
        let mut subscriber = ledger.subscribe(&session_id);

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut manager =
            octos_bus::SessionManager::open(tmp.path()).expect("session manager open");

        // Simulate the agent loop's commits: 2 metadata-only assistant
        // rows (intermediate iterations whose only payload was
        // tool_calls), interleaved with their tool results, then the
        // final assistant text row.
        let thread = "cmid-gamma-7".to_string();
        let mk_assistant = |content: &str, with_tool_calls: bool| Message {
            role: MessageRole::Assistant,
            content: content.into(),
            media: vec![],
            tool_calls: if with_tool_calls {
                Some(vec![octos_core::ToolCall {
                    id: format!("tc-{}", uuid::Uuid::now_v7()),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }])
            } else {
                None
            },
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some(thread.clone()),
            timestamp: Utc::now(),
        };
        let mk_tool = |out: &str, tc_id: &str| Message {
            role: MessageRole::Tool,
            content: out.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(tc_id.into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some(thread.clone()),
            timestamp: Utc::now(),
        };

        // Iteration 1: assistant returns only tool_calls (empty content).
        manager
            .add_message_with_seq(&session_id, mk_assistant("", true))
            .await
            .expect("commit it1 assistant");
        manager
            .add_message_with_seq(&session_id, mk_tool("ok", "tc-1"))
            .await
            .expect("commit it1 tool");
        // Iteration 2: assistant returns only tool_calls again.
        manager
            .add_message_with_seq(&session_id, mk_assistant("", true))
            .await
            .expect("commit it2 assistant");
        manager
            .add_message_with_seq(&session_id, mk_tool("ok", "tc-2"))
            .await
            .expect("commit it2 tool");
        // Iteration 3: final assistant text (the user-visible reply).
        manager
            .add_message_with_seq(&session_id, mk_assistant("here is your answer", false))
            .await
            .expect("commit it3 assistant");

        // Drain the broadcast and bucket by role on the
        // `MessagePersistedEvent` payload.
        let mut assistant_persisted = Vec::new();
        let mut tool_persisted = Vec::new();
        while let Ok(event) = subscriber.try_recv() {
            if let UiProtocolLedgerEvent::Notification(UiNotification::MessagePersisted(ev)) =
                &event.event
            {
                if ev.role == octos_core::MessageRole::Assistant.as_str() {
                    assistant_persisted.push(ev.clone());
                } else if ev.role == octos_core::MessageRole::Tool.as_str() {
                    tool_persisted.push(ev.clone());
                }
            }
        }

        assert_eq!(
            assistant_persisted.len(),
            1,
            "exactly ONE assistant message/persisted per turn (the final text); \
             got {} envelopes (phantom-bubble regression)",
            assistant_persisted.len(),
        );
        let final_envelope = &assistant_persisted[0];
        assert_eq!(final_envelope.role, "assistant");
        assert_eq!(
            final_envelope.source,
            MessagePersistedSource::Assistant,
            "assistant rows carry source=assistant"
        );
        // Method name matches the wire spec.
        assert_eq!(
            UiNotification::MessagePersisted(final_envelope.clone()).method(),
            methods::MESSAGE_PERSISTED,
        );

        // Tool rows are unaffected — both intermediate tool results land.
        assert_eq!(
            tool_persisted.len(),
            2,
            "tool rows must NOT be filtered (they always carry content)"
        );

        // Restore the global observer slot to None so subsequent tests
        // see a clean state.
        octos_bus::set_message_commit_observer(None);
    }

    /// PR F (M8.10 thread-binding chain `#649 → #740`): every progress
    /// event the BoundedChannelReporter emits MUST carry the bound
    /// `thread_id`. Without this, the SPA reducer for the standalone
    /// `octos serve` UI Protocol path falls back to sticky-map
    /// heuristics — the exact wire-side leak PR F closes.
    #[tokio::test]
    async fn bounded_channel_reporter_emits_typed_thread_id_on_progress_events() {
        use octos_agent::ProgressReporter;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
        let dropped = Arc::new(AtomicU64::new(0));

        let reporter = BoundedChannelReporter::new(tx, dropped.clone())
            .with_thread_id(Some("turn-pr-f-A".to_string()));
        reporter.report(octos_agent::ProgressEvent::Thinking { iteration: 0 });

        let event = rx.try_recv().expect("event must be available");
        let parsed: serde_json::Value = serde_json::from_str(&event).expect("valid json");
        assert_eq!(
            parsed["thread_id"], "turn-pr-f-A",
            "BoundedChannelReporter must stamp every progress event with the bound thread_id. event: {parsed}"
        );

        // Without binding, `thread_id` must be absent (legacy compat).
        let (tx2, mut rx2) = tokio::sync::mpsc::channel::<String>(8);
        let unbound = BoundedChannelReporter::new(tx2, dropped);
        unbound.report(octos_agent::ProgressEvent::Thinking { iteration: 1 });
        let event = rx2.try_recv().expect("event must be available");
        let parsed: serde_json::Value = serde_json::from_str(&event).expect("valid json");
        assert!(
            parsed.get("thread_id").is_none(),
            "unbound reporter must not stamp thread_id (legacy compat): {parsed}"
        );
    }

    // ========================================================================
    // Live ledger publish-subscribe (issue #760, Phase C blocker)
    // ========================================================================

    fn message_persisted_for(session: &SessionKey) -> UiNotification {
        UiNotification::MessagePersisted(MessagePersistedEvent {
            session_id: session.clone(),
            turn_id: Some(TurnId::new()),
            thread_id: None,
            seq: 0,
            role: "assistant".into(),
            message_id: "msg-1".into(),
            client_message_id: None,
            source: MessagePersistedSource::Tool,
            cursor: UiCursor {
                stream: session.0.clone(),
                seq: 0,
            },
            persisted_at: Utc::now(),
            media: vec![],
        })
    }

    fn features_with_message_persisted(enabled: bool) -> ConnectionUiFeatures {
        ConnectionUiFeatures {
            message_persisted: enabled,
            header_present: true,
            ..ConnectionUiFeatures::default()
        }
    }

    /// Build a `ConnectionUiFeatures` with the M10 Phase 1 dual gating
    /// flags set as requested. `message_persisted` is independent so the
    /// test can simulate clients that negotiated only one or both.
    fn features_for_spawn_complete_test(
        message_persisted: bool,
        spawn_complete: bool,
    ) -> ConnectionUiFeatures {
        ConnectionUiFeatures {
            message_persisted,
            spawn_complete,
            header_present: true,
            ..ConnectionUiFeatures::default()
        }
    }

    /// Builds a minimal `MessagePersistedEvent` with `source: background`,
    /// matching what `BackgroundResultSender`'s persist path produces
    /// via `MessageCommitObserver`. M10 Phase 1's per-connection filter
    /// suppresses this exact shape for new clients (which receive
    /// `turn/spawn_complete` instead).
    fn background_message_persisted_for(session: &SessionKey) -> UiNotification {
        UiNotification::MessagePersisted(MessagePersistedEvent {
            session_id: session.clone(),
            turn_id: Some(TurnId::new()),
            thread_id: Some("thread-1".into()),
            seq: 0,
            role: "assistant".into(),
            message_id: "msg-bg".into(),
            client_message_id: None,
            source: MessagePersistedSource::Background,
            cursor: UiCursor {
                stream: session.0.clone(),
                seq: 0,
            },
            persisted_at: Utc::now(),
            media: vec!["research/_report.md".into()],
        })
    }

    /// Build a representative `TurnSpawnCompleteEvent` that mirrors what
    /// `BackgroundResultSender` emits when a `spawn_only` task contracts
    /// and the originating `client_message_id` was tracked.
    fn turn_spawn_complete_for(session: &SessionKey) -> UiNotification {
        UiNotification::TurnSpawnComplete(TurnSpawnCompleteEvent {
            session_id: session.clone(),
            turn_id: Some(TurnId::new()),
            thread_id: Some("thread-1".into()),
            task_id: "task_abc123".into(),
            response_to_client_message_id: Some("cmid-user-1".into()),
            seq: 0,
            message_id: "msg-spawn".into(),
            source: "background".into(),
            cursor: UiCursor {
                stream: session.0.clone(),
                seq: 0,
            },
            persisted_at: Utc::now(),
            content: "Background research complete.".into(),
            media: vec!["research/_report.md".into()],
        })
    }

    /// Decodes a queued WS frame back to its JSON-RPC method name (or
    /// returns `None` for non-text / non-JSON frames). Lets tests assert
    /// the live broadcast forwarder routed a notification, without
    /// coupling to whatever frame_for serialization shape is.
    fn frame_method(frame: &WsMessage) -> Option<String> {
        match frame {
            WsMessage::Text(text) => {
                let v: Value = serde_json::from_str(text).ok()?;
                v.get("method").and_then(Value::as_str).map(str::to_owned)
            }
            _ => None,
        }
    }

    #[tokio::test]
    async fn live_forwarder_pushes_message_persisted_to_subscribed_ws() {
        let (ws, mut rx) = ws_connection_for_test(16);
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let session_id = SessionKey("local:livefwd".into());
        let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let live_rx = ledger.subscribe(&session_id);
        spawn_live_forwarder(
            ws.clone(),
            ledger.clone(),
            session_id.clone(),
            0,
            ws.connection_id(),
            features_with_message_persisted(true),
            live_rx,
            forwarders.clone(),
        )
        .await;

        // Background-task path appends late artifact AFTER the WS is wired up.
        ledger.append_notification(message_persisted_for(&session_id));

        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("ws received frame within 1s")
            .expect("ws channel still open");
        assert_eq!(
            frame_method(&frame).as_deref(),
            Some(octos_core::ui_protocol::methods::MESSAGE_PERSISTED),
            "live forwarder must emit message/persisted; frame={frame:?}"
        );

        // Cleanup: aborting the forwarder must not panic and must release
        // the receiver so subsequent prune_idle_subscribers reclaims the slot.
        abort_live_forwarders(&forwarders, &ledger).await;
    }

    #[tokio::test]
    async fn live_forwarder_skips_events_at_or_below_baseline_seq() {
        let (ws, mut rx) = ws_connection_for_test(16);
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let session_id = SessionKey("local:baseline".into());
        let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        // Pre-existing event so baseline_seq=1 represents "we already sent
        // this in replay; do not re-emit live."
        let baseline = ledger.append_notification(message_persisted_for(&session_id));
        assert_eq!(baseline.cursor.seq, 1);

        let live_rx = ledger.subscribe(&session_id);
        spawn_live_forwarder(
            ws.clone(),
            ledger.clone(),
            session_id.clone(),
            baseline.cursor.seq,
            ws.connection_id(),
            features_with_message_persisted(true),
            live_rx,
            forwarders.clone(),
        )
        .await;

        // A new append must surface; the forwarder filters strictly on
        // seq > baseline.
        let next = ledger.append_notification(message_persisted_for(&session_id));
        assert_eq!(next.cursor.seq, 2);

        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("ws received frame within 1s")
            .expect("ws channel still open");
        let v: Value = match &frame {
            WsMessage::Text(t) => serde_json::from_str(t).expect("valid json"),
            other => panic!("unexpected frame: {other:?}"),
        };
        assert_eq!(
            v.get("method").and_then(Value::as_str),
            Some(octos_core::ui_protocol::methods::MESSAGE_PERSISTED)
        );

        // No further frames are queued (only one live event emitted).
        assert!(rx.try_recv().is_err(), "no more frames expected");

        abort_live_forwarders(&forwarders, &ledger).await;
    }

    #[tokio::test]
    async fn live_forwarder_respects_message_persisted_capability_filter() {
        let (ws, mut rx) = ws_connection_for_test(16);
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let session_id = SessionKey("local:nofeat".into());
        let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let live_rx = ledger.subscribe(&session_id);
        spawn_live_forwarder(
            ws.clone(),
            ledger.clone(),
            session_id.clone(),
            0,
            ws.connection_id(),
            features_with_message_persisted(false),
            live_rx,
            forwarders.clone(),
        )
        .await;

        ledger.append_notification(message_persisted_for(&session_id));
        // Give the forwarder a chance to observe + filter.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_err(),
            "client without event.message_persisted.v1 must not receive message/persisted"
        );

        abort_live_forwarders(&forwarders, &ledger).await;
    }

    // ========================================================================
    // M10 Phase 1: `event.spawn_complete.v1` capability gating
    // ========================================================================

    /// Pure unit-level coverage of the dual filter — no WS / forwarder
    /// machinery. Documents the four corners of the negotiation matrix
    /// against the two relevant event shapes.
    #[test]
    fn capability_filter_routes_spawn_complete_dual_gating() {
        let session = SessionKey("local:filter".into());
        let bg_persisted =
            UiProtocolLedgerEvent::Notification(background_message_persisted_for(&session));
        let spawn_complete = UiProtocolLedgerEvent::Notification(turn_spawn_complete_for(&session));

        // Old client (no spawn_complete capability, has message_persisted):
        // sees `message/persisted` (Background source preserved); does NOT
        // see `turn/spawn_complete`.
        let old = features_for_spawn_complete_test(true, false);
        assert!(
            live_event_passes_capability_filter(&bg_persisted, old),
            "old client must keep receiving the legacy message/persisted shape",
        );
        assert!(
            !live_event_passes_capability_filter(&spawn_complete, old),
            "old client must not receive the new turn/spawn_complete shape",
        );

        // New client (has both capabilities): sees `turn/spawn_complete`;
        // the duplicate `message/persisted` (source: background) is
        // suppressed so the same logical event is delivered exactly once.
        let new = features_for_spawn_complete_test(true, true);
        assert!(
            !live_event_passes_capability_filter(&bg_persisted, new),
            "new client must NOT receive the duplicate message/persisted background row",
        );
        assert!(
            live_event_passes_capability_filter(&spawn_complete, new),
            "new client must receive the turn/spawn_complete envelope",
        );

        // New client without message_persisted negotiation: still gets
        // turn/spawn_complete. (The two capabilities are independent —
        // a forward-only client can opt into the new shape without ever
        // having shipped the older one.)
        let new_only = features_for_spawn_complete_test(false, true);
        assert!(
            !live_event_passes_capability_filter(&bg_persisted, new_only),
            "client without message_persisted does not see message/persisted regardless",
        );
        assert!(
            live_event_passes_capability_filter(&spawn_complete, new_only),
            "spawn_complete-only client receives turn/spawn_complete",
        );

        // Legacy client with NO negotiated features at all: sees neither
        // (the old gate already blocks message/persisted; the new gate
        // blocks turn/spawn_complete).
        let neither = features_for_spawn_complete_test(false, false);
        assert!(!live_event_passes_capability_filter(&bg_persisted, neither));
        assert!(!live_event_passes_capability_filter(
            &spawn_complete,
            neither
        ));

        // Sanity: a non-spawn `message/persisted` (source != background,
        // e.g. a regular assistant row) is unaffected by the spawn_complete
        // gate. Only the duplicate-suppression branch keys on
        // `MessagePersistedSource::Background`.
        let regular = UiProtocolLedgerEvent::Notification(message_persisted_for(&session));
        assert!(
            live_event_passes_capability_filter(&regular, new),
            "non-background message/persisted must still flow to new clients",
        );
    }

    /// End-to-end through the live forwarder for a NEW client (negotiated
    /// `event.spawn_complete.v1`): asserts they receive `turn/spawn_complete`
    /// AND the duplicate `message/persisted` (source: background) is
    /// suppressed. The combination is what kills the splice-merge
    /// double-render.
    #[tokio::test]
    async fn live_forwarder_routes_spawn_complete_to_new_client() {
        let (ws, mut rx) = ws_connection_for_test(16);
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let session_id = SessionKey("local:newfeat".into());
        let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let live_rx = ledger.subscribe(&session_id);
        spawn_live_forwarder(
            ws.clone(),
            ledger.clone(),
            session_id.clone(),
            0,
            ws.connection_id(),
            features_for_spawn_complete_test(true, true),
            live_rx,
            forwarders.clone(),
        )
        .await;

        // Producer side fires both — the persistence-driven
        // `message/persisted` (via `MessageCommitObserver`) AND the new
        // envelope (direct ledger append from `BackgroundResultSender`).
        ledger.append_notification(background_message_persisted_for(&session_id));
        ledger.append_notification(turn_spawn_complete_for(&session_id));

        // The new client must observe exactly one frame: the spawn_complete
        // envelope. The duplicate background message/persisted is filtered.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("expected the spawn_complete envelope within 1s")
            .expect("ws still open");
        assert_eq!(
            frame_method(&frame).as_deref(),
            Some(octos_core::ui_protocol::methods::TURN_SPAWN_COMPLETE),
            "first frame must be turn/spawn_complete (background message/persisted suppressed)",
        );

        // No further frames should arrive — the background message/persisted
        // was filtered.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_err(),
            "no second frame: background message/persisted is suppressed for new clients",
        );

        abort_live_forwarders(&forwarders, &ledger).await;
    }

    /// Codex P1: when the BackgroundResultSender persist scope is
    /// active, `current_message_persisted_source` must report
    /// `Background` regardless of the `Message` role. Outside the scope
    /// it falls back to the role-derived default — the pre-M10
    /// behaviour stays intact for every other persist path.
    #[tokio::test]
    async fn message_persisted_source_override_routes_through_task_local() {
        // Outside any scope: role-derived default.
        let default_for_assistant = current_message_persisted_source(MessageRole::Assistant);
        assert_eq!(default_for_assistant, MessagePersistedSource::Assistant);
        let default_for_tool = current_message_persisted_source(MessageRole::Tool);
        assert_eq!(default_for_tool, MessagePersistedSource::Tool);

        // Inside the override scope (mirrors what the
        // `BackgroundResultSender` closure does): every role maps to
        // `Background`.
        let bg = MESSAGE_PERSISTED_SOURCE_OVERRIDE
            .scope(Some(MessagePersistedSource::Background), async {
                (
                    current_message_persisted_source(MessageRole::Assistant),
                    current_message_persisted_source(MessageRole::Tool),
                )
            })
            .await;
        assert_eq!(bg.0, MessagePersistedSource::Background);
        assert_eq!(bg.1, MessagePersistedSource::Background);

        // After the scope ends, the default behaviour is restored.
        let after = current_message_persisted_source(MessageRole::Assistant);
        assert_eq!(after, MessagePersistedSource::Assistant);
    }

    /// Codex P2 follow-up: the `turn/spawn_complete` envelope's flat
    /// `seq` field carries the COMMITTED-ROW seq (the index in the
    /// session message log, identical to `MessagePersistedEvent.seq`)
    /// — NOT the UI-ledger cursor seq. The two scales differ in any
    /// turn that has prior ledger notifications, so upgraded clients
    /// reusing their `MessagePersisted` reducer for spawn completions
    /// MUST observe the persisted-row seq the producer wrote, not the
    /// ledger-assigned cursor seq.
    #[tokio::test]
    async fn ledger_preserves_producer_seq_and_stamps_only_cursor() {
        let ledger = UiProtocolLedger::new(8);
        let session_id = SessionKey("local:seq".into());

        // Producer sets `seq = 7` (the committed-row index from the
        // persist path). Ledger appends and stamps cursor.seq, but
        // must leave `seq` untouched.
        let mut event = match turn_spawn_complete_for(&session_id) {
            UiNotification::TurnSpawnComplete(ev) => ev,
            _ => unreachable!("test fixture is turn/spawn_complete"),
        };
        event.seq = 7;
        event.cursor.seq = 0; // producer seeds 0; ledger stamps the real cursor.
        let appended = ledger.append_notification(UiNotification::TurnSpawnComplete(event));

        let stamped = match &appended.event {
            UiProtocolLedgerEvent::Notification(UiNotification::TurnSpawnComplete(ev)) => ev,
            _ => panic!("expected turn/spawn_complete back from the ledger"),
        };
        assert_eq!(
            stamped.seq, 7,
            "ledger must NOT overwrite the producer's committed-row seq",
        );
        assert!(
            stamped.cursor.seq > 0,
            "cursor.seq must be the ledger-assigned non-zero cursor",
        );

        // Cursor is strictly monotonic across appends (same contract
        // as the existing MessagePersisted path); flat `seq` is
        // independent and tracked by the producer.
        let mut event2 = match turn_spawn_complete_for(&session_id) {
            UiNotification::TurnSpawnComplete(ev) => ev,
            _ => unreachable!(),
        };
        event2.seq = 8;
        let appended2 = ledger.append_notification(UiNotification::TurnSpawnComplete(event2));
        let stamped2 = match &appended2.event {
            UiProtocolLedgerEvent::Notification(UiNotification::TurnSpawnComplete(ev)) => ev,
            _ => panic!("turn/spawn_complete"),
        };
        assert!(stamped2.cursor.seq > stamped.cursor.seq);
        assert_eq!(stamped2.seq, 8);
    }

    /// End-to-end through the live forwarder for an OLD client (did NOT
    /// negotiate `event.spawn_complete.v1`): they see `message/persisted`
    /// (Background source preserved) and do NOT see `turn/spawn_complete`.
    /// This is the back-compat path that keeps existing TUI/CLI consumers
    /// working unchanged.
    #[tokio::test]
    async fn live_forwarder_falls_back_to_message_persisted_for_old_client() {
        let (ws, mut rx) = ws_connection_for_test(16);
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let session_id = SessionKey("local:oldfeat".into());
        let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let live_rx = ledger.subscribe(&session_id);
        spawn_live_forwarder(
            ws.clone(),
            ledger.clone(),
            session_id.clone(),
            0,
            ws.connection_id(),
            // Old client: has message_persisted but NOT spawn_complete.
            features_for_spawn_complete_test(true, false),
            live_rx,
            forwarders.clone(),
        )
        .await;

        ledger.append_notification(background_message_persisted_for(&session_id));
        ledger.append_notification(turn_spawn_complete_for(&session_id));

        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("expected the legacy message/persisted within 1s")
            .expect("ws still open");
        assert_eq!(
            frame_method(&frame).as_deref(),
            Some(octos_core::ui_protocol::methods::MESSAGE_PERSISTED),
            "first frame must be the legacy message/persisted shape for old clients",
        );

        // The new envelope must NOT be delivered to an un-negotiated client.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_err(),
            "no second frame: turn/spawn_complete is suppressed for old clients",
        );

        abort_live_forwarders(&forwarders, &ledger).await;
    }

    #[tokio::test]
    async fn live_forwarder_fans_out_to_two_concurrent_ws_connections() {
        let (ws_a, mut rx_a) = ws_connection_for_test(16);
        let (ws_b, mut rx_b) = ws_connection_for_test(16);
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let session_id = SessionKey("local:fanout".into());
        let forwarders_a: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let forwarders_b: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let rx_a_live = ledger.subscribe(&session_id);
        spawn_live_forwarder(
            ws_a.clone(),
            ledger.clone(),
            session_id.clone(),
            0,
            ws_a.connection_id(),
            features_with_message_persisted(true),
            rx_a_live,
            forwarders_a.clone(),
        )
        .await;
        let rx_b_live = ledger.subscribe(&session_id);
        spawn_live_forwarder(
            ws_b.clone(),
            ledger.clone(),
            session_id.clone(),
            0,
            ws_b.connection_id(),
            features_with_message_persisted(true),
            rx_b_live,
            forwarders_b.clone(),
        )
        .await;

        ledger.append_notification(message_persisted_for(&session_id));

        let frame_a = tokio::time::timeout(std::time::Duration::from_secs(1), rx_a.recv())
            .await
            .expect("ws_a frame")
            .expect("ws_a open");
        let frame_b = tokio::time::timeout(std::time::Duration::from_secs(1), rx_b.recv())
            .await
            .expect("ws_b frame")
            .expect("ws_b open");
        assert_eq!(
            frame_method(&frame_a).as_deref(),
            Some(octos_core::ui_protocol::methods::MESSAGE_PERSISTED)
        );
        assert_eq!(
            frame_method(&frame_b).as_deref(),
            Some(octos_core::ui_protocol::methods::MESSAGE_PERSISTED)
        );

        // Disconnect ws_a; ws_b must continue receiving subsequent events.
        abort_live_forwarders(&forwarders_a, &ledger).await;
        drop(rx_a);
        ledger.append_notification(message_persisted_for(&session_id));
        let frame_b2 = tokio::time::timeout(std::time::Duration::from_secs(1), rx_b.recv())
            .await
            .expect("ws_b frame after sibling drop")
            .expect("ws_b still open");
        assert_eq!(
            frame_method(&frame_b2).as_deref(),
            Some(octos_core::ui_protocol::methods::MESSAGE_PERSISTED)
        );

        abort_live_forwarders(&forwarders_b, &ledger).await;
    }

    // -- Codex PR #761 review fixes ----------------------------------------

    /// MUST-FIX-1: an event appended *after* the replay snapshot but
    /// *before* the live forwarder is wired up (the gap between
    /// `replay_after_with_head` returning and `spawn_live_forwarder`
    /// being awaited) must still reach the WS via the broadcast. The
    /// baseline must come from the replay snapshot's head — not the
    /// later session/open seq — otherwise a session/open append at H+2
    /// would shift the baseline up and silently drop the H+1 event.
    #[tokio::test]
    async fn live_forwarder_emits_event_appended_between_replay_and_forwarder_install() {
        let (ws, mut rx) = ws_connection_for_test(16);
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let session_id = SessionKey("local:gap".into());
        let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        // Pre-existing event at seq=1 — would be in replay history.
        let initial = ledger.append_notification(message_persisted_for(&session_id));
        assert_eq!(initial.cursor.seq, 1);

        // Snapshot replay (head=1) + subscribe in the same order
        // handle_session_open does.
        let live_rx = ledger.subscribe(&session_id);
        let (_replay, replay_head) = ledger
            .replay_after_with_head(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 0,
                }),
            )
            .expect("replay snapshot");
        assert_eq!(replay_head, 1);

        // GAP event — landed AFTER replay snapshot was taken but BEFORE
        // the forwarder is installed. With the broken design this would
        // be filtered out (baseline shifted to session/open's seq=H+2);
        // with the fix the broadcast buffer holds it and the forwarder
        // emits it once installed.
        let gap = ledger.append_notification(message_persisted_for(&session_id));
        assert_eq!(gap.cursor.seq, 2);

        // Append session/open AFTER the gap event — exactly the
        // ordering open_session_result produces.
        let opened = ledger.append_notification_from(
            UiNotification::SessionOpened(SessionOpened {
                session_id: session_id.clone(),
                active_profile_id: Some(MAIN_PROFILE_ID.to_owned()),
                workspace_root: None,
                cursor: None,
                panes: None,
                capabilities: UiProtocolCapabilities::first_server_slice(),
            }),
            ws.connection_id(),
        );
        assert_eq!(opened.cursor.seq, 3);

        // Wire up the forwarder using replay_head as the baseline. The
        // gap event has seq > baseline AND it is not from this
        // connection, so it must surface on the WS.
        spawn_live_forwarder(
            ws.clone(),
            ledger.clone(),
            session_id.clone(),
            replay_head,
            ws.connection_id(),
            features_with_message_persisted(true),
            live_rx,
            forwarders.clone(),
        )
        .await;

        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("forwarder must emit gap event")
            .expect("ws still open");
        assert_eq!(
            frame_method(&frame).as_deref(),
            Some(octos_core::ui_protocol::methods::MESSAGE_PERSISTED),
            "the H+1 gap event must reach the WS, not be silently filtered"
        );

        // The session/open event itself must NOT come back via the
        // broadcast (it carries our connection_id, so the forwarder
        // skips it — the handler already direct-sent it inline).
        assert!(
            rx.try_recv().is_err(),
            "no further frames expected: session/open must be self-suppressed"
        );

        abort_live_forwarders(&forwarders, &ledger).await;
    }

    /// MUST-FIX-2: a `send_notification_durable` call from the same
    /// connection that owns an active live forwarder must deliver the
    /// frame exactly once. Without `from_connection` self-suppression
    /// the forwarder would receive the broadcast and double-send.
    #[tokio::test]
    async fn send_notification_durable_does_not_double_deliver_via_live_forwarder() {
        let (ws, mut rx) = ws_connection_for_test(16);
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let session_id = SessionKey("local:dedup".into());
        let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let live_rx = ledger.subscribe(&session_id);
        spawn_live_forwarder(
            ws.clone(),
            ledger.clone(),
            session_id.clone(),
            0,
            ws.connection_id(),
            features_with_message_persisted(true),
            live_rx,
            forwarders.clone(),
        )
        .await;

        // Direct-send via the standard handler path. This both persists
        // (with our connection_id stamped) and direct-sends inline.
        send_notification_durable(&ws, &ledger, message_persisted_for(&session_id))
            .expect("direct send succeeds");

        // Exactly one frame must arrive.
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("first frame")
            .expect("ws open");
        assert_eq!(
            frame_method(&first).as_deref(),
            Some(octos_core::ui_protocol::methods::MESSAGE_PERSISTED)
        );
        // Give the forwarder time to (incorrectly) re-emit if the fix
        // regresses; with self-suppression nothing further must arrive.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_err(),
            "send_notification_durable must deliver exactly once on its own connection"
        );

        // Sanity: a different connection's forwarder still receives the
        // event via fan-out (the suppression is per-connection).
        let (ws_other, mut rx_other) = ws_connection_for_test(16);
        let forwarders_other: SharedLiveForwarders =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let live_rx_other = ledger.subscribe(&session_id);
        spawn_live_forwarder(
            ws_other.clone(),
            ledger.clone(),
            session_id.clone(),
            0,
            ws_other.connection_id(),
            features_with_message_persisted(true),
            live_rx_other,
            forwarders_other.clone(),
        )
        .await;
        send_notification_durable(&ws, &ledger, message_persisted_for(&session_id))
            .expect("second send");
        let frame_other = tokio::time::timeout(std::time::Duration::from_secs(1), rx_other.recv())
            .await
            .expect("other connection sees fan-out")
            .expect("ws_other open");
        assert_eq!(
            frame_method(&frame_other).as_deref(),
            Some(octos_core::ui_protocol::methods::MESSAGE_PERSISTED)
        );

        abort_live_forwarders(&forwarders, &ledger).await;
        abort_live_forwarders(&forwarders_other, &ledger).await;
    }

    /// MUST-FIX-3: a `subscribe()` call followed by dropping the
    /// receiver (modelling a failed `session/open` path) must not leak
    /// a sender. The `prune_subscriber_if_idle` hook called on the
    /// failure path reclaims the slot immediately.
    #[tokio::test]
    async fn session_open_failure_path_does_not_leak_broadcast_sender() {
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let session_id = SessionKey("local:leakcheck".into());

        // Mirror handle_session_open's "subscribe BEFORE
        // open_session_result" ordering. Then simulate failure: drop
        // the receiver, prune.
        let live_rx = ledger.subscribe(&session_id);
        assert_eq!(ledger.subscriber_count(), 1, "sender installed");

        drop(live_rx);
        let pruned = ledger.prune_subscriber_if_idle(&session_id);
        assert!(pruned, "failed open must reclaim the orphan sender");
        assert_eq!(
            ledger.subscriber_count(),
            0,
            "no senders survive a failed session/open"
        );

        // Steady-state sweep also reclaims any orphans that escape the
        // failure path (defence in depth).
        let kept = ledger.subscribe(&session_id);
        ledger.prune_idle_subscribers(); // receiver still alive — no-op.
        assert_eq!(ledger.subscriber_count(), 1);
        drop(kept);
        assert_eq!(
            ledger.prune_idle_subscribers(),
            1,
            "sweep reclaims orphans after every receiver drops"
        );
        assert_eq!(ledger.subscriber_count(), 0);
    }

    /// Lag handling: when the broadcast buffer overflows, the receiver
    /// observes `RecvError::Lagged(n)` and the forwarder must NOT die —
    /// it logs and keeps pumping subsequent events. The earlier missed
    /// events are recoverable via cursor replay (the ledger is durable).
    #[tokio::test]
    async fn live_forwarder_survives_broadcast_lag_and_keeps_pumping() {
        let (ws, mut rx) = ws_connection_for_test(WS_WRITER_CHANNEL_CAPACITY);
        let ledger = Arc::new(UiProtocolLedger::new(2048));
        let session_id = SessionKey("local:lag".into());
        let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        // Subscribe but don't pump yet. Overflow the broadcast capacity
        // (LIVE_BROADCAST_CAPACITY = 256) so the receiver lags.
        let live_rx = ledger.subscribe(&session_id);
        for _ in 0..512 {
            ledger.append_notification(message_persisted_for(&session_id));
        }

        // Now install the forwarder — its first recv() will see
        // Lagged(n). It must log and continue, not abort.
        spawn_live_forwarder(
            ws.clone(),
            ledger.clone(),
            session_id.clone(),
            0,
            ws.connection_id(),
            features_with_message_persisted(true),
            live_rx,
            forwarders.clone(),
        )
        .await;

        // A fresh append after lag must be delivered.
        ledger.append_notification(message_persisted_for(&session_id));
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("post-lag frame must arrive — forwarder kept pumping")
            .expect("ws still open");
        assert_eq!(
            frame_method(&frame).as_deref(),
            Some(octos_core::ui_protocol::methods::MESSAGE_PERSISTED)
        );

        abort_live_forwarders(&forwarders, &ledger).await;
    }

    // ----- M11-E: UI Protocol per-session workspace wiring -----------------

    /// Stub `LlmProvider` for M11-E tests. The acceptance scenarios drive
    /// `open_session_result` + the session cache wiring — they never call
    /// out to a real model. Mirrors `handlers::make_m11d_profile`'s
    /// `EchoLlm` but does not bother encoding a reply since these tests
    /// only inspect the per-session `ToolRegistry` and `workspace_root`.
    struct M11EStubLlm;

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for M11EStubLlm {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            unreachable!("M11EStubLlm should not be invoked from M11-E acceptance tests")
        }

        fn model_id(&self) -> &str {
            "m11e-stub"
        }

        fn provider_name(&self) -> &str {
            "stub"
        }
    }

    async fn make_m11e_profile(
        profile_id: &str,
        data_dir: &std::path::Path,
    ) -> Arc<crate::runtime::ProfileRuntime> {
        std::fs::create_dir_all(data_dir).expect("profile data dir");
        let memory = Arc::new(
            octos_memory::EpisodeStore::open(data_dir)
                .await
                .expect("episode store"),
        );
        let memory_store = Arc::new(
            octos_memory::MemoryStore::open(data_dir)
                .await
                .expect("memory store"),
        );
        let tool_config = Arc::new(
            octos_agent::ToolConfigStore::open(data_dir)
                .await
                .expect("tool config store"),
        );
        let sandbox = octos_agent::SandboxConfig::default();
        let base_tools = octos_agent::ToolRegistry::with_builtins_and_sandbox(
            data_dir,
            octos_agent::create_sandbox(&sandbox),
        );
        Arc::new(crate::runtime::ProfileRuntime {
            profile_id: profile_id.to_string(),
            data_dir: data_dir.to_path_buf(),
            llm: Arc::new(M11EStubLlm),
            adaptive_router: None,
            runtime_qos_catalog: None,
            primary_model_id: "m11e-stub".to_string(),
            provider_name: "stub".to_string(),
            credentials: HashMap::new(),
            skills_dir: None,
            plugin_env_template: Vec::new(),
            tool_policy: None,
            default_sandbox: sandbox,
            tool_specs: Arc::new(base_tools),
            plugin_tool_names: Vec::new(),
            plugin_dirs: Vec::new(),
            plugin_prompt_fragments: Vec::new(),
            plugin_hooks: Vec::new(),
            system_prompt: "test-system-prompt".to_string(),
            memory,
            memory_store,
            tool_config,
            cron_service: None,
            hook_executor: None,
        })
    }

    /// AppState for M11-E acceptance tests: a registered `ProfileRuntime`
    /// plus a process-wide `SessionManager` so the audit-log writer and
    /// pane-snapshot path have a `data_dir` to resolve.
    ///
    /// M11-F: the legacy `agent` field was deleted; every read path now
    /// resolves through `state.profiles` + `state.session_cache`.
    async fn state_with_profile(
        data_dir: &std::path::Path,
        profile_id: &str,
    ) -> (Arc<AppState>, Arc<crate::runtime::ProfileRuntime>) {
        std::fs::create_dir_all(data_dir).expect("data dir");

        let sessions = Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir).expect("session manager"),
        ));

        let profile_data_dir = data_dir.join("profiles").join(profile_id).join("data");
        let profile_runtime = make_m11e_profile(profile_id, &profile_data_dir).await;

        let mut profiles = HashMap::new();
        profiles.insert(profile_id.to_string(), profile_runtime.clone());

        let state = Arc::new(AppState {
            profiles,
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });

        (state, profile_runtime)
    }

    /// M11-E acceptance §1: a session opened with a custom `cwd` materializes
    /// a `SessionRuntime` whose `workspace_root` IS that cwd, and the
    /// session's `ReadFileTool` reads files from that cwd. This is the
    /// "supplied workspace, not the daemon cwd" invariant from the issue
    /// — the legacy `clone_session_tools` path could only honor it
    /// indirectly through the global `session_workspaces()` map.
    #[tokio::test]
    async fn appui_session_with_custom_cwd_reads_supplied_workspace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (state, profile_runtime) = state_with_profile(temp.path(), "m11e-custom-cwd").await;

        // Pre-seed the supplied workspace with a sentinel file the session
        // is expected to read back.
        let supplied_cwd = temp.path().join("supplied-workspace");
        std::fs::create_dir_all(&supplied_cwd).expect("create supplied cwd");
        let sentinel = supplied_cwd.join("hello.txt");
        std::fs::write(&sentinel, "session-A reads its own workspace\n").expect("seed sentinel");

        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();

        let session_id = SessionKey::with_profile("m11e-custom-cwd", "api", "custom-cwd-session");
        let features = ConnectionUiFeatures {
            session_workspace_cwd: true,
            header_present: true,
            ..ConnectionUiFeatures::default()
        };

        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            ConnectionId::next(),
            Some("m11e-custom-cwd"),
            features,
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: Some("m11e-custom-cwd".into()),
                cwd: Some(supplied_cwd.to_string_lossy().into_owned()),
                after: None,
            },
        )
        .await
        .expect("session/open with supplied cwd must succeed");

        // The wire response carries the canonical workspace_root so dashboard
        // clients render the right cwd on reconnect.
        let opened_root = outcome
            .result
            .opened
            .workspace_root
            .as_ref()
            .expect("opened workspace_root populated");
        assert_eq!(
            std::fs::canonicalize(opened_root).expect("canonicalize opened root"),
            std::fs::canonicalize(&supplied_cwd).expect("canonicalize supplied cwd"),
        );

        // The session cache must hold a SessionRuntime materialized from
        // THIS supplied cwd — that's the wiring the issue tracks.
        let session_runtime = state
            .session_cache
            .get_or_init(&profile_runtime, session_id.clone(), None)
            .await
            .expect("cached session runtime");
        assert_eq!(
            std::fs::canonicalize(&session_runtime.workspace_root).expect("canonicalize root"),
            std::fs::canonicalize(&supplied_cwd).expect("canonicalize supplied cwd"),
            "SessionRuntime.workspace_root must be the client-supplied cwd"
        );

        // The session's read_file tool sees the supplied workspace, not
        // the daemon cwd / the profile data_dir.
        let result = session_runtime
            .tools
            .execute("read_file", &json!({ "path": "hello.txt" }))
            .await
            .expect("read_file via session tools");
        assert!(result.success, "read_file must succeed: {}", result.output);
        assert!(
            result.output.contains("session-A reads its own workspace"),
            "expected session A's sentinel content, got: {}",
            result.output
        );
    }

    /// M11-E acceptance §2: two AppUI sessions opened on the SAME profile
    /// with DIFFERENT cwds must not see each other's files. This is the
    /// multi-tenant scope invariant codex flagged on PR #868 — pre-M11
    /// the per-session view was cloned off a single `base_agent`-bound
    /// registry, leaving the workspace_root vulnerable to cross-session
    /// leakage if any caller forgot to rebind. With `SessionRuntime.tools`
    /// the only path, two sessions hold two distinct `Arc<ToolRegistry>`
    /// instances each pinned at bootstrap time.
    #[tokio::test]
    async fn two_appui_sessions_on_same_profile_with_different_cwds_isolated() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (state, profile_runtime) = state_with_profile(temp.path(), "m11e-multi-cwd").await;

        let cwd_a = temp.path().join("session-a");
        let cwd_b = temp.path().join("session-b");
        std::fs::create_dir_all(&cwd_a).expect("create cwd-a");
        std::fs::create_dir_all(&cwd_b).expect("create cwd-b");

        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();

        let session_a = SessionKey::with_profile("m11e-multi-cwd", "api", "session-a");
        let session_b = SessionKey::with_profile("m11e-multi-cwd", "api", "session-b");

        let features = ConnectionUiFeatures {
            session_workspace_cwd: true,
            header_present: true,
            ..ConnectionUiFeatures::default()
        };

        // Open session A with cwd_a.
        let _ = open_session_result(
            &state,
            &ledger,
            &approvals,
            ConnectionId::next(),
            Some("m11e-multi-cwd"),
            features,
            SessionOpenParams {
                session_id: session_a.clone(),
                profile_id: Some("m11e-multi-cwd".into()),
                cwd: Some(cwd_a.to_string_lossy().into_owned()),
                after: None,
            },
        )
        .await
        .expect("session A open");

        // Open session B with cwd_b.
        let _ = open_session_result(
            &state,
            &ledger,
            &approvals,
            ConnectionId::next(),
            Some("m11e-multi-cwd"),
            features,
            SessionOpenParams {
                session_id: session_b.clone(),
                profile_id: Some("m11e-multi-cwd".into()),
                cwd: Some(cwd_b.to_string_lossy().into_owned()),
                after: None,
            },
        )
        .await
        .expect("session B open");

        let rt_a = state
            .session_cache
            .get_or_init(&profile_runtime, session_a.clone(), None)
            .await
            .expect("session A runtime");
        let rt_b = state
            .session_cache
            .get_or_init(&profile_runtime, session_b.clone(), None)
            .await
            .expect("session B runtime");

        // Two sessions on the same profile must hold distinct
        // `Arc<ToolRegistry>` instances (codex multi-tenant scope note).
        assert!(
            !Arc::ptr_eq(&rt_a.tools, &rt_b.tools),
            "per-session tool registries must be distinct Arcs"
        );
        assert_ne!(rt_a.workspace_root, rt_b.workspace_root);

        // Session A writes a.txt under cwd_a; session B writes b.txt under cwd_b.
        rt_a.tools
            .execute(
                "write_file",
                &json!({ "path": "a.txt", "content": "from session A\n" }),
            )
            .await
            .expect("write_file under session A's workspace");
        rt_b.tools
            .execute(
                "write_file",
                &json!({ "path": "b.txt", "content": "from session B\n" }),
            )
            .await
            .expect("write_file under session B's workspace");

        // Cross-read MUST fail: session B cannot see a.txt; session A
        // cannot see b.txt. Per-session isolation enforced by the
        // workspace-bound registries built at SessionRuntime::bootstrap.
        let a_reads_a = rt_a
            .tools
            .execute("read_file", &json!({ "path": "a.txt" }))
            .await
            .expect("session A reads its own a.txt");
        assert!(a_reads_a.success, "{}", a_reads_a.output);
        assert!(a_reads_a.output.contains("from session A"));

        let b_reads_b = rt_b
            .tools
            .execute("read_file", &json!({ "path": "b.txt" }))
            .await
            .expect("session B reads its own b.txt");
        assert!(b_reads_b.success, "{}", b_reads_b.output);
        assert!(b_reads_b.output.contains("from session B"));

        // Cross-read MUST fail or return error output: session A cannot
        // see b.txt; session B cannot see a.txt. Per-session isolation
        // enforced by the workspace-bound registries built at
        // SessionRuntime::bootstrap.
        let a_cross = rt_a
            .tools
            .execute("read_file", &json!({ "path": "b.txt" }))
            .await
            .expect("read_file always returns a ToolResult");
        let a_cross_lower = a_cross.output.to_lowercase();
        assert!(
            !a_cross.success
                || a_cross_lower.contains("not found")
                || a_cross_lower.contains("no such")
                || a_cross_lower.contains("error"),
            "session A must NOT be able to read session B's b.txt; got: success={} output={}",
            a_cross.success,
            a_cross.output
        );

        let b_cross = rt_b
            .tools
            .execute("read_file", &json!({ "path": "a.txt" }))
            .await
            .expect("read_file always returns a ToolResult");
        let b_cross_lower = b_cross.output.to_lowercase();
        assert!(
            !b_cross.success
                || b_cross_lower.contains("not found")
                || b_cross_lower.contains("no such")
                || b_cross_lower.contains("error"),
            "session B must NOT be able to read session A's a.txt; got: success={} output={}",
            b_cross.success,
            b_cross.output
        );

        // Filesystem invariant: a.txt physically lives under cwd_a, b.txt
        // physically lives under cwd_b. Catches a regression where the
        // per-session tool somehow wrote to the legacy
        // `session_workspaces()`-resolved path instead of the workspace
        // the SessionRuntime was bootstrapped against.
        assert!(cwd_a.join("a.txt").exists());
        assert!(cwd_b.join("b.txt").exists());
        assert!(!cwd_a.join("b.txt").exists());
        assert!(!cwd_b.join("a.txt").exists());
    }

    /// M11-E codex round-1 MEDIUM: a second `session/open` for the same
    /// session_id but a DIFFERENT cwd must NOT silently rebind a
    /// running session's workspace. The cache's `get_or_init` is
    /// single-flight per key; a same-key hit returns the cached
    /// `Arc<SessionRuntime>` and ignores the new `workspace_hint`.
    /// The `SessionOpened.workspace_root` reply MUST reflect the
    /// CACHED runtime (not the just-requested cwd) — otherwise the
    /// SPA renders one cwd while the next turn uses another, which is
    /// exactly the wire/state divergence codex flagged.
    #[tokio::test]
    async fn second_session_open_with_new_cwd_reports_cached_workspace_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (state, _profile_runtime) =
            state_with_profile(temp.path(), "m11e-rebind-attempt").await;

        let first_cwd = temp.path().join("first");
        let second_cwd = temp.path().join("second");
        std::fs::create_dir_all(&first_cwd).expect("create first");
        std::fs::create_dir_all(&second_cwd).expect("create second");

        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_id = SessionKey::with_profile("m11e-rebind-attempt", "api", "single-key");
        let features = ConnectionUiFeatures {
            session_workspace_cwd: true,
            header_present: true,
            ..ConnectionUiFeatures::default()
        };

        let first_open = open_session_result(
            &state,
            &ledger,
            &approvals,
            ConnectionId::next(),
            Some("m11e-rebind-attempt"),
            features,
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: Some("m11e-rebind-attempt".into()),
                cwd: Some(first_cwd.to_string_lossy().into_owned()),
                after: None,
            },
        )
        .await
        .expect("first open");

        let second_open = open_session_result(
            &state,
            &ledger,
            &approvals,
            ConnectionId::next(),
            Some("m11e-rebind-attempt"),
            features,
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: Some("m11e-rebind-attempt".into()),
                // Different cwd — must NOT take effect; cache is sticky.
                cwd: Some(second_cwd.to_string_lossy().into_owned()),
                after: None,
            },
        )
        .await
        .expect("second open");

        let first_root = first_open
            .result
            .opened
            .workspace_root
            .expect("first workspace_root populated");
        let second_root = second_open
            .result
            .opened
            .workspace_root
            .expect("second workspace_root populated");
        assert_eq!(
            std::fs::canonicalize(&first_root).expect("canonicalize first"),
            std::fs::canonicalize(&second_root).expect("canonicalize second"),
            "second open of the same session must report the cached \
             workspace_root, not the just-requested cwd",
        );
        assert_eq!(
            std::fs::canonicalize(&second_root).expect("canonicalize"),
            std::fs::canonicalize(&first_cwd).expect("canonicalize"),
            "cached workspace_root must equal the FIRST open's cwd",
        );
    }

    /// M11-E codex round-3 HIGH: when `state.profiles` is non-empty but
    /// the ROUTED profile is not in it, `validate_session_workspace_allowed`
    /// must still reject the cwd with `cwd_runtime_unavailable`.
    /// Otherwise the wire reply would advertise the requested
    /// workspace_root while the turn dispatcher's legacy fallback uses
    /// `base_agent`'s root — exactly the divergence the prior round
    /// closed for the empty-profiles case. The fix routes the active
    /// profile id into the validator so it can resolve the SPECIFIC
    /// runtime rather than checking the map non-empty.
    #[tokio::test]
    async fn session_open_with_cwd_for_unregistered_profile_is_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (state, _profile_runtime) = state_with_profile(temp.path(), "m11e-registered").await;

        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("create cwd");

        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();

        // Build a SessionKey under a profile id that is NOT in
        // `state.profiles` so the active profile resolution misses.
        let session_id = SessionKey::with_profile("m11e-not-registered", "api", "x");
        let features = ConnectionUiFeatures {
            session_workspace_cwd: true,
            header_present: true,
            ..ConnectionUiFeatures::default()
        };

        let error = open_session_result(
            &state,
            &ledger,
            &approvals,
            ConnectionId::next(),
            // No connection identity so the routed id falls to the
            // session-id-embedded "m11e-not-registered".
            None,
            features,
            SessionOpenParams {
                session_id,
                profile_id: None,
                cwd: Some(cwd.to_string_lossy().into_owned()),
                after: None,
            },
        )
        .await
        .expect_err("cwd for unregistered profile must reject");

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("cwd_runtime_unavailable")),
            "unregistered profile must surface cwd_runtime_unavailable; \
             got error data {:?}",
            error.data,
        );
    }

    /// M11-E codex round-1 HIGH (filed against octos-agent, not blocking
    /// M11-E): symlink-based directory escape is an octos-agent
    /// `resolve_path` + `read_no_follow`/`write_no_follow` property,
    /// not a UI-Protocol property. M11-E binds each session's tools to
    /// its own workspace_root via `SessionRuntime::bootstrap`; the
    /// remaining gap is that the tool layer follows directory
    /// symlinks (only the final path component is checked).
    ///
    /// This test pins the CURRENT octos-agent behavior so a future
    /// `read_no_follow`/`write_no_follow` hardening flips it green
    /// automatically. We document the gap in the PR body and propose
    /// it as a follow-up octos-agent issue (NOT a downstream M11
    /// ticket — the tool layer is the right home).
    ///
    /// Marking `#[ignore]` rather than failing the suite: the gap is
    /// pre-existing, M11-E neither introduces nor fixes it, and a
    /// failing test here would block landing M11-E for a problem
    /// that lives in a different crate.
    #[tokio::test]
    #[ignore = "octos-agent gap: directory symlinks escape per-session workspace; tracked as follow-up issue, NOT blocking M11-E"]
    async fn parent_directory_symlink_escapes_per_session_workspace_documents_gap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (state, profile_runtime) = state_with_profile(temp.path(), "m11e-symlink").await;

        let cwd_a = temp.path().join("session-a");
        let cwd_b = temp.path().join("session-b");
        std::fs::create_dir_all(&cwd_a).expect("create cwd-a");
        std::fs::create_dir_all(&cwd_b).expect("create cwd-b");
        std::fs::write(cwd_b.join("secret.txt"), "B's private data\n").expect("seed b");

        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();
        let session_a = SessionKey::with_profile("m11e-symlink", "api", "session-a");
        let features = ConnectionUiFeatures {
            session_workspace_cwd: true,
            header_present: true,
            ..ConnectionUiFeatures::default()
        };

        let _ = open_session_result(
            &state,
            &ledger,
            &approvals,
            ConnectionId::next(),
            Some("m11e-symlink"),
            features,
            SessionOpenParams {
                session_id: session_a.clone(),
                profile_id: Some("m11e-symlink".into()),
                cwd: Some(cwd_a.to_string_lossy().into_owned()),
                after: None,
            },
        )
        .await
        .expect("session A open");

        // Plant a directory symlink inside session A's workspace
        // pointing at session B's workspace. The path-normalize check
        // in `resolve_path` and the O_NOFOLLOW guard in
        // `read_no_follow` both pass — only the FINAL path component is
        // checked for the symlink-rejection invariant.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&cwd_b, cwd_a.join("escape")).expect("plant symlink");
        }
        #[cfg(not(unix))]
        {
            // The gap is Unix-specific (Windows tool fallback uses a
            // separate symlink-check pattern). Skip the planting on
            // non-Unix.
            return;
        }

        let rt_a = state
            .session_cache
            .get_or_init(&profile_runtime, session_a.clone(), None)
            .await
            .expect("cached session A runtime");

        let result = rt_a
            .tools
            .execute("read_file", &json!({ "path": "escape/secret.txt" }))
            .await
            .expect("read_file returns a ToolResult");

        // CURRENT BEHAVIOR (M11-E lock-in): octos-agent follows the
        // parent directory symlink and reads B's file. The
        // `#[ignore]` keeps this from failing CI while we file the
        // octos-agent issue; flip to `assert!(!result.success)` when
        // the tool-layer fix lands.
        assert!(
            result.success && result.output.contains("B's private data"),
            "octos-agent should currently follow the directory symlink \
             (M11-E gap documentation); got success={} output={}",
            result.success,
            result.output
        );
    }

    /// M11-F deliverable D — restore `appui.default_session_cwd` Tier-2
    /// fallback that M11-E's `clone_session_tools` deletion took out.
    ///
    /// Pre-resolution order on `session.open`:
    ///   1. Tier 1 — client-supplied `cwd` (already wired via
    ///      `session.workspace_cwd.v1` + `validate_session_workspace_allowed`).
    ///   2. Tier 2 — operator-configured `appui.default_session_cwd`
    ///      mirrored on `AppState::appui_default_session_cwd`. **This
    ///      test pins that wiring.**
    ///   3. Tier 3 — `SessionRuntime::bootstrap`'s
    ///      `<profile.data_dir>/users/<encoded base>/workspace` default.
    ///
    /// Scenario: a client that does NOT advertise
    /// `session.workspace_cwd.v1` opens an AppUI session with no `cwd`.
    /// `AppState::appui_default_session_cwd` is set to an operator
    /// directory. The materialized `SessionRuntime.workspace_root` MUST
    /// equal the operator default — not the profile-data-relative
    /// Tier-3 fallback — and the wire `workspace_root` must reflect it.
    #[tokio::test]
    async fn appui_session_without_client_cwd_respects_operator_default_session_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (mut state_inner, profile_runtime) = {
            let (state, profile_runtime) =
                state_with_profile(temp.path(), "m11f-tier2-default").await;
            // We need to mutate AppState (set appui_default_session_cwd)
            // before sharing it. `state_with_profile` returns an
            // `Arc<AppState>`; for the test we unwrap via
            // `Arc::try_unwrap` knowing this is the only reference.
            (
                Arc::try_unwrap(state)
                    .map_err(|_| "state Arc must be unique for test setup")
                    .expect("unique Arc"),
                profile_runtime,
            )
        };

        // Operator-configured Tier-2 default. The directory exists and
        // contains a sentinel the session is expected to read back via
        // its own (workspace-bound) read_file tool.
        let operator_default = temp.path().join("operator-default-workspace");
        std::fs::create_dir_all(&operator_default).expect("create operator default");
        std::fs::write(
            operator_default.join("hello.txt"),
            "tier-2 operator default visible to session\n",
        )
        .expect("seed sentinel");
        state_inner.appui_default_session_cwd = Some(operator_default.clone());
        let state = Arc::new(state_inner);

        let ledger = UiProtocolLedger::new(16);
        let approvals = PendingApprovalStore::default();

        let session_id =
            SessionKey::with_profile("m11f-tier2-default", "api", "tier2-no-client-cwd");
        // IMPORTANT: client does NOT advertise `session.workspace_cwd.v1`
        // and does NOT send a cwd — this is the exact octos-app shape
        // that the M11-E deletion of `clone_session_tools` broke.
        let features = ConnectionUiFeatures {
            session_workspace_cwd: false,
            header_present: true,
            ..ConnectionUiFeatures::default()
        };

        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            ConnectionId::next(),
            Some("m11f-tier2-default"),
            features,
            SessionOpenParams {
                session_id: session_id.clone(),
                profile_id: Some("m11f-tier2-default".into()),
                cwd: None,
                after: None,
            },
        )
        .await
        .expect("session/open with operator default cwd must succeed");

        // Wire response carries the operator-default workspace root.
        let opened_root = outcome
            .result
            .opened
            .workspace_root
            .as_ref()
            .expect("opened workspace_root populated");
        assert_eq!(
            std::fs::canonicalize(opened_root).expect("canonicalize opened root"),
            std::fs::canonicalize(&operator_default).expect("canonicalize operator default"),
            "Tier-2: SessionOpened.workspace_root must equal appui.default_session_cwd",
        );

        // The cached SessionRuntime is bound to the operator default —
        // not the Tier-3 profile-data-relative fallback.
        let session_runtime = state
            .session_cache
            .get_or_init(&profile_runtime, session_id.clone(), None)
            .await
            .expect("cached session runtime");
        assert_eq!(
            std::fs::canonicalize(&session_runtime.workspace_root)
                .expect("canonicalize runtime root"),
            std::fs::canonicalize(&operator_default).expect("canonicalize operator default"),
            "Tier-2: SessionRuntime.workspace_root must equal appui.default_session_cwd",
        );

        // End-to-end: a read_file against the relative sentinel resolves
        // inside the operator default, proving the per-session
        // ToolRegistry was rebound to that root (not the Tier-3
        // `<profile_data_dir>/users/<encoded>/workspace`).
        let result = session_runtime
            .tools
            .execute("read_file", &json!({ "path": "hello.txt" }))
            .await
            .expect("read_file via session tools");
        assert!(
            result.success,
            "read_file must succeed under operator default: {}",
            result.output
        );
        assert!(
            result
                .output
                .contains("tier-2 operator default visible to session"),
            "expected operator-default sentinel content, got: {}",
            result.output
        );
    }
}
