//! Session actor: per-session tokio task that owns tools and processes messages.
//!
//! Replaces the spawn-per-message model in the gateway, eliminating the
//! `set_context()` race condition where shared tools could route messages
//! to the wrong chat.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use metrics::counter;
use octos_agent::compaction::CompactionRunner;
use octos_agent::tools::spawn::{
    ChildSessionFailureAction, ChildSessionLifecycleKind, ChildSessionLifecyclePayload,
};
use octos_agent::tools::{
    BackgroundResultKind, BackgroundResultPayload, CheckBackgroundTasksTool, MessageTool,
    SendFileTool, SpawnTool, ToolPolicy, ToolRegistry,
};
use octos_agent::{
    Agent, AgentConfig, CompactionSummarizerKind, HookContext, HookExecutor, HookPayload,
    HookResult, LoopRetryState, TaskSupervisor, TokenTracker, TurnAttachmentContext,
    WorkspacePolicy, read_workspace_policy, workspace_policy_path, write_workspace_policy,
};
use octos_bus::{
    ActiveSessionStore, SessionHandle, SessionManager,
    session::{
        ChildSessionContract, ChildSessionFailureAction as PersistedChildSessionFailureAction,
        ChildSessionJoinState, ChildSessionTerminalState,
    },
};
use octos_core::AgentId;
use octos_core::{
    InboundMessage, MAIN_PROFILE_ID, METADATA_SENDER_USER_ID, Message, MessageRole,
    OutboundMessage, SessionKey,
};
use octos_llm::{
    AdaptiveMode, AdaptiveRouter, EmbeddingProvider, LlmProvider, ProviderRouter,
    ResponsivenessObserver, pricing::model_pricing,
};
use octos_memory::{EpisodeStore, MemoryStore};
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::QueueMode;
use crate::cron_tool::CronTool;
use crate::status_layers::{StatusComposer, UserStatusConfig};
use crate::workflow_runtime::{WorkflowInstance, WorkflowKind};

/// Parameters for dispatching an inbound message to a session actor.
pub struct DispatchParams<'a> {
    pub message: InboundMessage,
    pub image_media: Vec<String>,
    pub attachment_media: Vec<String>,
    pub attachment_prompt: Option<String>,
    pub session_key: SessionKey,
    pub reply_channel: &'a str,
    pub reply_chat_id: &'a str,
    pub status_indicator: Option<Arc<StatusComposer>>,
    pub profile_id: Option<&'a str>,
    pub system_prompt_override: Option<String>,
    pub sender_user_id: Option<String>,
}

/// Parameters for spawning a new session actor.
struct SpawnParams<'a> {
    session_key: SessionKey,
    channel: &'a str,
    chat_id: &'a str,
    semaphore: Arc<Semaphore>,
    status_indicator: Option<Arc<StatusComposer>>,
    system_prompt_override: Option<String>,
    sender_user_id: Option<String>,
}

/// Parameters for the outbound message forwarder task.
struct ForwarderParams {
    proxy_rx: mpsc::Receiver<OutboundMessage>,
    out_tx: mpsc::Sender<OutboundMessage>,
    session_key: SessionKey,
    channel: String,
    chat_id: String,
    active_sessions: Arc<RwLock<ActiveSessionStore>>,
    pending_messages: PendingMessages,
    sender_user_id: Option<String>,
}

/// Default actor inbox capacity.
const ACTOR_INBOX_SIZE: usize = 32;

/// Default idle timeout before an actor shuts down (30 minutes).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1800;

/// Maximum concurrent overflow tasks per session.
const MAX_OVERFLOW_TASKS: u32 = 5;

/// Maximum number of pending messages buffered per inactive session.
const MAX_PENDING_PER_SESSION: usize = 50;

/// Bound actor inbox send/ack waits for background terminal delivery.
const BACKGROUND_RESULT_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound live outbound fanout so persistence never waits indefinitely on a slow channel.
const BACKGROUND_RESULT_FANOUT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, serde::Serialize)]
struct PersistedSessionMessage {
    seq: usize,
    timestamp: chrono::DateTime<chrono::Utc>,
}

/// Review A F-015: resolve the JSON sidecar path for a session's persistent
/// retry-bucket state. Lives under `{data_dir}/sessions/retry_state_{id}.json`
/// where `id` is a filesystem-safe hash of the session key. A collision-free
/// URL-safe encoding would be more correct, but SHA-256 over the raw key is
/// stable, short, and avoids any weird characters so we prefer it.
fn retry_state_sidecar_path(
    data_dir: &std::path::Path,
    session_key: &SessionKey,
) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(session_key.0.as_bytes());
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // 16 hex chars = 64 bits: plenty for per-user collision resistance and
    // keeps the filename short. Full digest is available via debug logs.
    let short = &hex[..16];
    data_dir
        .join("sessions")
        .join(format!("retry_state_{short}.json"))
}

/// Review A F-015: read a session's persistent `LoopRetryState` from disk.
/// Returns `LoopRetryState::default()` when the file is missing, empty,
/// unreadable, or malformed — the schema is advisory (the state is safe to
/// reset; the only downside is losing cross-turn accumulation for that
/// session).
fn load_retry_state(path: &std::path::Path) -> LoopRetryState {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return LoopRetryState::default();
    };
    match serde_json::from_str::<LoopRetryState>(&raw) {
        Ok(state) => state,
        Err(error) => {
            warn!(
                path = %path.display(),
                error = %error,
                "retry_state sidecar is malformed; starting fresh"
            );
            LoopRetryState::default()
        }
    }
}

/// Review A F-015: write the session's persistent retry state to disk via
/// the atomic write-then-rename dance already used by session JSONL files.
/// Silently logs failures — the sidecar is best-effort durability and must
/// never block the agent loop.
fn save_retry_state(path: &std::path::Path, state: &LoopRetryState) {
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            warn!(
                path = %parent.display(),
                error = %error,
                "failed to create retry_state sidecar directory"
            );
            return;
        }
    }
    let serialized = match serde_json::to_string_pretty(state) {
        Ok(value) => value,
        Err(error) => {
            warn!(
                path = %path.display(),
                error = %error,
                "failed to serialize retry_state sidecar"
            );
            return;
        }
    };
    let tmp_path = path.with_extension("json.tmp");
    if let Err(error) = std::fs::write(&tmp_path, serialized) {
        warn!(
            path = %tmp_path.display(),
            error = %error,
            "failed to write retry_state sidecar (tmp)"
        );
        return;
    }
    if let Err(error) = std::fs::rename(&tmp_path, path) {
        warn!(
            tmp = %tmp_path.display(),
            path = %path.display(),
            error = %error,
            "failed to rename retry_state sidecar into place"
        );
    }
}

async fn persist_assistant_message(
    session_handle: &Arc<Mutex<SessionHandle>>,
    session_key: &SessionKey,
    data_dir: &Path,
    content: String,
    media: Vec<String>,
    thread_id: Option<String>,
) -> Option<PersistedSessionMessage> {
    let mut assistant_msg = Message::assistant(content);
    assistant_msg.media = media;
    // M8.10 follow-up (#649): pre-stamp `thread_id` BEFORE handing the
    // message to the canonical persist helper. `add_message_with_seq`'s
    // derivation falls back to "most recent user in history" — for a
    // late-arriving background result that's the WRONG user (a later turn
    // that happened after the originating one). When the caller knows the
    // originating turn (background results carry `originating_thread_id`
    // through `BackgroundResultPayload`), passing it here pins the
    // persisted JSONL row to the correct thread so reload pairs the
    // assistant under the originating user bubble. Foreground turns pass
    // `None` and continue to use the derivation fallback (correct).
    if let Some(tid) = thread_id {
        if !tid.is_empty() {
            assistant_msg.thread_id = Some(tid);
        }
    }
    let timestamp = assistant_msg.timestamp;

    // Funnel through the canonical helper so the per-key Tokio mutex
    // serialises this write with `ApiChannel::persist_to_session` (and any
    // other caller). Pre-fix, the actor opened its OWN `SessionHandle` and
    // called `add_message_with_seq` directly — the channel and actor each
    // observed their independent in-memory `len = N`, both returned the
    // same `seq = N`, and the duplicate seqs broke watcher correlation.
    //
    // Holding `session_handle.lock()` across the canonical-helper call is
    // safe (the helper's per-key map is independent of the actor's per-actor
    // mutex; no deadlock) and serialises this write with the actor's other
    // in-memory operations (read history, summary update, etc.). After the
    // disk write commits we mirror the message into the actor's local Vec
    // so subsequent `get_history` reads stay consistent.
    let mut handle = session_handle.lock().await;
    match octos_bus::session::persist_message_through_canonical_path(
        data_dir,
        session_key,
        assistant_msg.clone(),
    )
    .await
    {
        Ok(seq) => {
            handle.push_message_in_memory(assistant_msg);
            Some(PersistedSessionMessage { seq, timestamp })
        }
        Err(error) => {
            warn!(
                session = %session_key,
                error = %error,
                "failed to persist assistant message"
            );
            None
        }
    }
}

/// Poll the session log briefly for the primary turn's assistant reply, then
/// return the freshest history snapshot.
///
/// This exists to fix a stale-history bug on the speculative-overflow path:
/// when a user sends a follow-up while the primary turn is still running, the
/// overflow agent used to read a snapshot taken BEFORE the primary turn
/// started, missing the answer the primary just produced. Polling for a new
/// assistant message lets the overflow re-use that fresh context.
///
/// `pre_primary_assistant_count` is the number of assistant messages observed
/// before the primary turn began. The loop exits once the live snapshot has
/// strictly more, or once the deadline elapses (so a slow primary never blocks
/// the overflow indefinitely — it just runs with whatever context it has).
async fn wait_for_primary_assistant_reply(
    session_handle: &Arc<Mutex<SessionHandle>>,
    max_history: usize,
    pre_primary_assistant_count: usize,
    max_wait: Duration,
    poll_interval: Duration,
) -> Vec<Message> {
    let deadline = Instant::now() + max_wait;
    loop {
        let snapshot: Vec<Message> = {
            let handle = session_handle.lock().await;
            handle.get_history(max_history).to_vec()
        };
        let cur_assistant_count = snapshot
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant))
            .count();
        if cur_assistant_count > pre_primary_assistant_count || Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// Read the optional `client_message_id` field from an InboundMessage's
/// metadata. Empty strings count as absent so the wire schema stays simple
/// for clients that always populate the field.
fn inbound_client_message_id(inbound: &InboundMessage) -> Option<String> {
    inbound
        .metadata
        .get("client_message_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn site_preview_url_for_session(session_key: &SessionKey, user_workspace: &Path) -> Option<String> {
    let topic = session_key.topic()?;
    let profile_id = session_key.profile_id().unwrap_or(MAIN_PROFILE_ID);
    let expected = crate::project_templates::build_site_project_metadata(
        profile_id,
        session_key.chat_id(),
        topic,
        user_workspace,
    )?;
    let project_dir = user_workspace.join(&expected.project_dir);
    crate::project_templates::read_site_project_metadata(&project_dir)
        .map(|metadata| metadata.preview_url)
        .or(Some(expected.preview_url))
        .filter(|value| !value.trim().is_empty())
}

fn finalize_assistant_content(
    session_key: &SessionKey,
    user_workspace: &Path,
    content: &str,
) -> String {
    let content = strip_invoke_tags(content).trim().to_string();
    let is_site = session_key
        .topic()
        .is_some_and(|topic| topic == "site" || topic.starts_with("site "));
    if !is_site || content.trim().is_empty() || content.contains("/api/preview/") {
        return content;
    }

    let Some(preview_url) = site_preview_url_for_session(session_key, user_workspace) else {
        return content;
    };

    format!("{content}\n\nPreview URL: {preview_url}")
}

async fn send_outbound_with_timeout(
    session_key: &SessionKey,
    out_tx: &mpsc::Sender<OutboundMessage>,
    message: OutboundMessage,
    fanout_kind: &'static str,
) -> bool {
    match tokio::time::timeout(BACKGROUND_RESULT_FANOUT_TIMEOUT, out_tx.send(message)).await {
        Ok(Ok(())) => {
            record_result_delivery(fanout_kind, "sent", "assistant");
            true
        }
        Ok(Err(error)) => {
            record_result_delivery(fanout_kind, "channel_closed", "assistant");
            warn!(
                session = %session_key,
                error = %error,
                fanout_kind,
                "failed to fan out outbound message"
            );
            false
        }
        Err(_) => {
            record_result_delivery(fanout_kind, "timeout", "assistant");
            warn!(
                session = %session_key,
                timeout_ms = BACKGROUND_RESULT_FANOUT_TIMEOUT.as_millis(),
                fanout_kind,
                "timed out while fanning out outbound message"
            );
            false
        }
    }
}

/// M8.10 PR #2: optional `thread_id` is the user message's
/// client_message_id. When present, the outbound the helper emits is
/// tagged with `thread_id` metadata so the API channel can stamp SSE
/// payloads with the correct per-cmid routing key.
#[allow(clippy::too_many_arguments)]
async fn persist_terminal_reply_and_fanout(
    session_handle: &Arc<Mutex<SessionHandle>>,
    session_key: &SessionKey,
    data_dir: &Path,
    out_tx: &mpsc::Sender<OutboundMessage>,
    channel: &str,
    chat_id: &str,
    reply_to: Option<String>,
    content: String,
    media: Vec<String>,
    thread_id: Option<&str>,
) -> bool {
    let Some(_persisted) = persist_assistant_message(
        session_handle,
        session_key,
        data_dir,
        content.clone(),
        media.clone(),
        thread_id.map(str::to_string),
    )
    .await
    else {
        record_result_delivery("terminal_reply", "history_not_persisted", "assistant");
        warn!(
            session = %session_key,
            "skipping live fanout because terminal reply was not persisted"
        );
        return false;
    };

    let mut metadata = serde_json::json!({});
    if let Some(tid) = thread_id {
        if !tid.is_empty() {
            if let Some(map) = metadata.as_object_mut() {
                map.insert(
                    "thread_id".to_string(),
                    serde_json::Value::String(tid.to_string()),
                );
            }
        }
    }

    send_outbound_with_timeout(
        session_key,
        out_tx,
        OutboundMessage {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            content,
            reply_to,
            media,
            metadata,
        },
        "terminal_reply",
    )
    .await
}

const CHILD_SESSION_HISTORY_COPY: usize = 6;

fn child_session_lifecycle_kind_label(kind: ChildSessionLifecycleKind) -> &'static str {
    match kind {
        ChildSessionLifecycleKind::Spawned => "spawned",
        ChildSessionLifecycleKind::Completed => "completed",
        ChildSessionLifecycleKind::RetryableFailed => "retryable_failed",
        ChildSessionLifecycleKind::TerminalFailed => "terminal_failed",
    }
}

fn record_child_session_lifecycle(kind: ChildSessionLifecycleKind, outcome: &'static str) {
    counter!(
        "octos_child_session_lifecycle_total",
        "kind" => child_session_lifecycle_kind_label(kind).to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

fn record_timeout(reason: &'static str) {
    counter!("octos_timeout_total", "reason" => reason.to_string()).increment(1);
}

fn record_retry(reason: &'static str) {
    counter!("octos_retry_total", "reason" => reason.to_string()).increment(1);
}

/// Collect per-node cost rows from a turn's tool-result side-channel
/// metadata into a flat array suitable for the SSE `done` event.
///
/// Bug 3 / W1.G4 — tools (today: `run_pipeline`) surface per-node cost
/// rows via `ToolResult.structured_metadata` keyed under `"node_costs"`.
/// The session actor walks every tool result and concatenates the rows so
/// the dashboard CostBreakdown panel sees one cost row per pipeline node
/// regardless of how many `run_pipeline` calls fired during the turn.
///
/// Returns an empty vector when no tool surfaced cost rows — the caller
/// only writes the `node_costs` key on the SSE event when this is
/// non-empty so legacy clients keep their byte-for-byte payload shape.
fn collect_node_costs(tool_results: &[(String, serde_json::Value)]) -> Vec<serde_json::Value> {
    let mut all_node_costs: Vec<serde_json::Value> = Vec::new();
    for (_tool_call_id, meta) in tool_results {
        if let Some(arr) = meta.get("node_costs").and_then(|v| v.as_array()) {
            all_node_costs.extend(arr.iter().cloned());
        }
    }
    all_node_costs
}

fn record_result_delivery(path: &'static str, outcome: &'static str, kind: &'static str) {
    counter!(
        "octos_result_delivery_total",
        "path" => path.to_string(),
        "outcome" => outcome.to_string(),
        "kind" => kind.to_string()
    )
    .increment(1);
}

fn child_session_spawn_note(payload: &ChildSessionLifecyclePayload) -> String {
    let mut lines = vec![
        format!(
            "[Background child session created for \"{}\"]",
            payload.task_label
        ),
        format!("Parent session: {}", payload.parent_session_key),
        format!("Child session: {}", payload.child_session_key),
    ];
    if let Some(ref workflow_kind) = payload.workflow_kind {
        lines.push(format!("Workflow: {workflow_kind}"));
    }
    if let Some(ref phase) = payload.current_phase {
        lines.push(format!("Phase: {phase}"));
    }
    lines.push(format!("Instruction: {}", payload.instruction));
    lines.join("\n")
}

fn child_session_terminal_state(
    kind: ChildSessionLifecycleKind,
) -> Option<ChildSessionTerminalState> {
    match kind {
        ChildSessionLifecycleKind::Completed => Some(ChildSessionTerminalState::Completed),
        ChildSessionLifecycleKind::RetryableFailed => {
            Some(ChildSessionTerminalState::RetryableFailure)
        }
        ChildSessionLifecycleKind::TerminalFailed => {
            Some(ChildSessionTerminalState::TerminalFailure)
        }
        ChildSessionLifecycleKind::Spawned => None,
    }
}

fn child_session_failure_action_label(action: ChildSessionFailureAction) -> &'static str {
    match action {
        ChildSessionFailureAction::Retry => "retry",
        ChildSessionFailureAction::Escalate => "escalate",
    }
}

fn persisted_child_session_failure_action(
    action: ChildSessionFailureAction,
) -> PersistedChildSessionFailureAction {
    match action {
        ChildSessionFailureAction::Retry => PersistedChildSessionFailureAction::Retry,
        ChildSessionFailureAction::Escalate => PersistedChildSessionFailureAction::Escalate,
    }
}

fn child_session_terminal_note(
    payload: &ChildSessionLifecyclePayload,
    join_state: ChildSessionJoinState,
) -> String {
    let mut lines = vec![match payload.kind {
        ChildSessionLifecycleKind::Completed => {
            format!("Background task \"{}\" completed.", payload.task_label)
        }
        ChildSessionLifecycleKind::RetryableFailed => {
            format!(
                "Background task \"{}\" failed and may be retried.",
                payload.task_label
            )
        }
        ChildSessionLifecycleKind::TerminalFailed => {
            format!("Background task \"{}\" failed.", payload.task_label)
        }
        ChildSessionLifecycleKind::Spawned => {
            format!("Background task \"{}\" spawned.", payload.task_label)
        }
    }];
    if let Some(ref workflow_kind) = payload.workflow_kind {
        lines.push(format!("Workflow: {workflow_kind}"));
    }
    if let Some(ref phase) = payload.current_phase {
        lines.push(format!("Phase: {phase}"));
    }
    lines.push(format!(
        "Join state: {}",
        match join_state {
            ChildSessionJoinState::Joined => "joined",
            ChildSessionJoinState::Orphaned => "orphaned",
        }
    ));
    if let Some(action) = payload.failure_action {
        lines.push(format!(
            "Failure action: {}",
            child_session_failure_action_label(action)
        ));
        lines.push(
            match action {
                ChildSessionFailureAction::Retry => {
                    "Next step: retry from the parent session when prerequisites recover."
                }
                ChildSessionFailureAction::Escalate => {
                    "Next step: escalate to the parent session or user; do not blindly retry."
                }
            }
            .to_string(),
        );
    }
    if !payload.output_files.is_empty() {
        lines.push("Output files:".to_string());
        lines.extend(payload.output_files.iter().map(|path| format!("- {path}")));
    }
    if let Some(ref error) = payload.error {
        lines.push(format!("Error: {error}"));
    }
    lines.join("\n")
}

async fn persist_child_session_lifecycle(
    data_dir: &Path,
    payload: &ChildSessionLifecyclePayload,
) -> eyre::Result<bool> {
    let parent_key = SessionKey(payload.parent_session_key.clone());
    let child_key = SessionKey(payload.child_session_key.clone());
    let parent_exists = SessionHandle::session_exists(data_dir, &parent_key);

    match payload.kind {
        ChildSessionLifecycleKind::Spawned => {
            SessionHandle::fork_from_parent_if_missing(
                data_dir,
                &parent_key,
                &child_key,
                CHILD_SESSION_HISTORY_COPY,
            )
            .await?;

            let note = child_session_spawn_note(payload);
            let mut child = SessionHandle::open(data_dir, &child_key);
            let exists = child
                .session()
                .messages
                .iter()
                .any(|message| message.role == MessageRole::System && message.content == note);
            if !exists {
                child.add_message(Message::system(note)).await?;
            }

            let contract = ChildSessionContract {
                task_id: payload.task_id.clone(),
                task_label: payload.task_label.clone(),
                parent_session_key: payload.parent_session_key.clone(),
                child_session_key: payload.child_session_key.clone(),
                workflow_kind: payload.workflow_kind.clone(),
                current_phase: payload.current_phase.clone(),
                terminal_state: None,
                join_state: None,
                joined_at: None,
                failure_action: None,
                error: None,
                output_files: Vec::new(),
            };
            let _ = child.upsert_child_contract(contract.clone()).await?;
            if parent_exists {
                let mut parent = SessionHandle::open(data_dir, &parent_key);
                let _ = parent.upsert_child_contract(contract).await?;
            }
            record_child_session_lifecycle(ChildSessionLifecycleKind::Spawned, "persisted");
            Ok(parent_exists)
        }
        ChildSessionLifecycleKind::Completed
        | ChildSessionLifecycleKind::RetryableFailed
        | ChildSessionLifecycleKind::TerminalFailed => {
            if parent_exists {
                SessionHandle::fork_from_parent_if_missing(
                    data_dir,
                    &parent_key,
                    &child_key,
                    CHILD_SESSION_HISTORY_COPY,
                )
                .await?;
            }
            let terminal_state = child_session_terminal_state(payload.kind)
                .expect("terminal child lifecycle should have a state");
            let join_state = if parent_exists {
                ChildSessionJoinState::Joined
            } else {
                ChildSessionJoinState::Orphaned
            };
            let note = child_session_terminal_note(payload, join_state.clone());
            let mut child = SessionHandle::open(data_dir, &child_key);
            let exists =
                child.session().messages.iter().any(|message| {
                    message.role == MessageRole::Assistant && message.content == note
                });
            if !exists {
                child.add_message(Message::assistant(note)).await?;
            }
            let contract = ChildSessionContract {
                task_id: payload.task_id.clone(),
                task_label: payload.task_label.clone(),
                parent_session_key: payload.parent_session_key.clone(),
                child_session_key: payload.child_session_key.clone(),
                workflow_kind: payload.workflow_kind.clone(),
                current_phase: payload.current_phase.clone(),
                terminal_state: Some(terminal_state),
                join_state: Some(join_state.clone()),
                joined_at: if matches!(join_state, ChildSessionJoinState::Joined) {
                    Some(chrono::Utc::now())
                } else {
                    None
                },
                failure_action: payload
                    .failure_action
                    .map(persisted_child_session_failure_action),
                error: payload.error.clone(),
                output_files: payload.output_files.clone(),
            };
            let _ = child.upsert_child_contract(contract.clone()).await?;
            if parent_exists {
                let mut parent = SessionHandle::open(data_dir, &parent_key);
                let _ = parent.upsert_child_contract(contract).await?;
            }
            record_child_session_lifecycle(
                payload.kind,
                if matches!(join_state, ChildSessionJoinState::Joined) {
                    "joined"
                } else {
                    "orphaned"
                },
            );
            Ok(matches!(join_state, ChildSessionJoinState::Joined))
        }
    }
}

fn resolve_builtin_slides_styles_dir(data_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let current_profile_id = data_dir
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());

    let family_root_profile = current_profile_id
        .as_deref()
        .and_then(|value| value.split("--").next())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());

    let octos_home = data_dir
        .ancestors()
        .nth(3)
        .map(std::path::Path::to_path_buf);

    let mut candidates = Vec::new();
    candidates.push(data_dir.join("skills").join("mofa-slides").join("styles"));

    if let Some(ref home) = octos_home {
        candidates.push(home.join("skills").join("mofa-slides").join("styles"));

        if let Some(ref root_profile) = family_root_profile {
            candidates.push(
                home.join("profiles")
                    .join(root_profile)
                    .join("data")
                    .join("skills")
                    .join("mofa-slides")
                    .join("styles"),
            );
        }
    }

    candidates.into_iter().find(|candidate| candidate.is_dir())
}

/// Shared buffer of outbound messages from inactive sessions, keyed by session key string.
/// Flushed when the user switches to that session via `/s`.
pub type PendingMessages = Arc<Mutex<HashMap<String, Vec<OutboundMessage>>>>;

/// Shared lookup table for session-scoped background task supervisors.
#[derive(Default, Clone)]
pub struct SessionTaskQueryStore {
    supervisors: Arc<StdMutex<HashMap<String, SessionTaskQueryEntry>>>,
}

struct SessionTaskQueryEntry {
    supervisor: Weak<TaskSupervisor>,
    data_dir: PathBuf,
}

fn task_response_path(data_dir: &Path, path: &str) -> String {
    octos_bus::file_handle::encode_profile_file_handle(data_dir, Path::new(path))
        .unwrap_or_else(|| path.to_string())
}

fn task_runtime_detail_for_response(
    detail: Option<&str>,
) -> (serde_json::Value, Option<String>, Option<String>) {
    let runtime_detail = match detail {
        Some(detail) => serde_json::from_str(detail)
            .unwrap_or_else(|_| serde_json::Value::String(detail.to_string())),
        None => serde_json::Value::Null,
    };
    let workflow_kind = runtime_detail
        .get("workflow_kind")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let current_phase = runtime_detail
        .get("current_phase")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    (runtime_detail, workflow_kind, current_phase)
}

fn sanitize_task_for_response(
    data_dir: &Path,
    task: &octos_agent::BackgroundTask,
) -> serde_json::Value {
    let (runtime_detail, workflow_kind, current_phase) =
        task_runtime_detail_for_response(task.runtime_detail.as_deref());
    serde_json::json!({
        "id": task.id,
        "tool_name": task.tool_name,
        "tool_call_id": task.tool_call_id,
        "parent_session_key": task.parent_session_key,
        "child_session_key": task.child_session_key,
        "status": task.status,
        "lifecycle_state": task.lifecycle_state(),
        "started_at": task.started_at,
        "updated_at": task.updated_at,
        "completed_at": task.completed_at,
        "runtime_state": task.runtime_state,
        "runtime_detail": runtime_detail,
        "workflow_kind": workflow_kind,
        "current_phase": current_phase,
        "child_terminal_state": task.child_terminal_state,
        "child_join_state": task.child_join_state,
        "child_joined_at": task.child_joined_at,
        "child_failure_action": task.child_failure_action,
        "output_files": task.output_files.iter().map(|path| task_response_path(data_dir, path)).collect::<Vec<_>>(),
        "error": task.error,
        "session_key": task.session_key,
    })
}

/// Forward a `BackgroundTask` snapshot from the supervisor's
/// `set_on_change` callback into the session actor's bounded inbox.
///
/// **Terminal updates** (`completed` / `failed` / `cancelled`) MUST NOT
/// be dropped under inbox backpressure — dropping one leaves any SSE /
/// UI consumer stuck on `running` (M9 review finding #6). On try_send
/// failure the helper upgrades to a spawned `tx.send().await` bounded
/// by [`BACKGROUND_RESULT_ACK_TIMEOUT`] so the update is durable
/// through transient backpressure but does not pile up zombies if the
/// actor is permanently gone.
///
/// **Non-terminal updates** are coalesce-friendly (the next update
/// overwrites) and stay on the non-blocking `try_send` fast-path.
fn forward_task_status_to_actor_inbox(
    tx: &tokio::sync::mpsc::Sender<ActorMessage>,
    data_dir: &Path,
    task: &octos_agent::BackgroundTask,
) {
    let task_json = sanitize_task_for_response(data_dir, task);
    let Ok(json) = serde_json::to_string(&task_json) else {
        return;
    };
    let msg = ActorMessage::TaskStatusChanged { task_json: json };
    let Err(tokio::sync::mpsc::error::TrySendError::Full(msg)) = tx.try_send(msg) else {
        // Either Ok (delivered) or Closed (actor gone — nothing to deliver to).
        return;
    };
    counter!(
        "session_actor.task_status.try_send.full",
        "terminal" => task.status.is_terminal().to_string()
    )
    .increment(1);
    if !task.status.is_terminal() {
        return;
    }
    let durable_tx = tx.clone();
    let task_id = task.id.clone();
    let lifecycle = task.lifecycle_state();
    tokio::spawn(async move {
        match tokio::time::timeout(BACKGROUND_RESULT_ACK_TIMEOUT, durable_tx.send(msg)).await {
            Ok(Ok(())) => {}
            Ok(Err(_send_err)) => {
                tracing::debug!(
                    target: "octos::session_actor",
                    %task_id,
                    ?lifecycle,
                    "terminal task_status_changed dropped: actor inbox closed"
                );
            }
            Err(_elapsed) => {
                counter!("session_actor.task_status.timeout.terminal").increment(1);
                tracing::warn!(
                    target: "octos::session_actor",
                    %task_id,
                    ?lifecycle,
                    timeout_ms = BACKGROUND_RESULT_ACK_TIMEOUT.as_millis() as u64,
                    "terminal task_status_changed timed out under sustained backpressure"
                );
            }
        }
    });
}

impl SessionTaskQueryStore {
    pub fn register(
        &self,
        session_key: &SessionKey,
        supervisor: &Arc<TaskSupervisor>,
        data_dir: &Path,
    ) {
        let mut guard = self.supervisors.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            session_key.to_string(),
            SessionTaskQueryEntry {
                supervisor: Arc::downgrade(supervisor),
                data_dir: data_dir.to_path_buf(),
            },
        );
    }

    /// Look up the live supervisor + data dir for `session_key`, pruning a
    /// stale entry if the underlying `Arc<TaskSupervisor>` has been dropped.
    fn lookup_live_supervisor(&self, session_key: &str) -> Option<(Arc<TaskSupervisor>, PathBuf)> {
        let mut guard = self.supervisors.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get(session_key).and_then(|entry| {
            entry
                .supervisor
                .upgrade()
                .map(|supervisor| (supervisor, entry.data_dir.clone()))
        }) {
            Some(entry) => Some(entry),
            None => {
                guard.remove(session_key);
                None
            }
        }
    }

    /// Return the JSON task list for `session_key` and every reachable
    /// descendant session. The walk follows each task's
    /// [`octos_agent::BackgroundTask::child_session_key`] to the next
    /// supervisor (when one is registered and still alive) so that, e.g., a
    /// `run_pipeline` task running inside a child session shows up in its
    /// parent's `/api/sessions/:id/tasks` view. Without this, UIs cannot
    /// correlate the parent's rendered tool_call_id bubble with the actual
    /// child-session task.
    ///
    /// Traversal is breadth-first with a `visited` guard so cycles or
    /// duplicate child keys do not trigger redundant work. Auth/ownership
    /// checks happen at the API layer for the parent — descendants inherit
    /// access by virtue of being spawned from the authorized parent.
    pub fn query_json(&self, session_key: &str) -> serde_json::Value {
        let mut tasks: Vec<serde_json::Value> = Vec::new();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        queue.push_back(session_key.to_string());
        visited.insert(session_key.to_string());

        while let Some(current) = queue.pop_front() {
            let Some((supervisor, data_dir)) = self.lookup_live_supervisor(&current) else {
                continue;
            };
            for task in supervisor.get_tasks_for_session(&current) {
                if let Some(child_key) = task.child_session_key.as_deref() {
                    if visited.insert(child_key.to_string()) {
                        queue.push_back(child_key.to_string());
                    }
                }
                tasks.push(sanitize_task_for_response(&data_dir, &task));
            }
        }

        serde_json::Value::Array(tasks)
    }

    /// M7.9 / W2: locate the supervisor owning `task_id` and forward
    /// `cancel(task_id)` to it. Returns `Ok(())` on success, mapping
    /// supervisor errors back to the typed [`TaskCancelError`] enum so
    /// the API layer can map them to HTTP status codes.
    ///
    /// Walks every live supervisor (pruning dropped ones) until it finds
    /// the task. When no supervisor knows about `task_id`, returns
    /// `Err(TaskCancelError::NotFound)`.
    pub fn cancel_task(&self, task_id: &str) -> Result<(), octos_agent::TaskCancelError> {
        for supervisor in self.live_supervisors() {
            if supervisor.get_task(task_id).is_some() {
                return supervisor.cancel(task_id);
            }
        }
        Err(octos_agent::TaskCancelError::NotFound)
    }

    /// M7.9 / W2: locate the supervisor owning `task_id` and forward
    /// `relaunch(task_id, opts)` to it. Returns `Ok(new_task_id)` on
    /// success.
    pub fn relaunch_task(
        &self,
        task_id: &str,
        opts: octos_agent::RelaunchOpts,
    ) -> Result<String, octos_agent::TaskRelaunchError> {
        for supervisor in self.live_supervisors() {
            if supervisor.get_task(task_id).is_some() {
                return supervisor.relaunch(task_id, opts);
            }
        }
        Err(octos_agent::TaskRelaunchError::NotFound)
    }

    /// Snapshot live supervisors, pruning dropped weak refs. Shared
    /// helper for `cancel_task` / `relaunch_task` /
    /// `mark_child_session_failed`.
    fn live_supervisors(&self) -> Vec<Arc<TaskSupervisor>> {
        let mut guard = self.supervisors.lock().unwrap_or_else(|e| e.into_inner());
        let mut alive = Vec::new();
        guard.retain(|_, entry| match entry.supervisor.upgrade() {
            Some(supervisor) => {
                alive.push(supervisor);
                true
            }
            None => false,
        });
        alive
    }

    /// M8 fix-first item 8 (gap 3): mark the parent task that owns a
    /// child session as failed.
    ///
    /// When a child session refuses to resume because its worktree has
    /// disappeared, the in-memory transcript is cleared as a safety floor
    /// (M8.6 fix-first item 3) but the parent task that spawned this
    /// child is left in `Running`. Dashboards then show a stuck task that
    /// will never make progress. This method walks every registered
    /// supervisor, looking for a `BackgroundTask` whose
    /// `child_session_key` matches `child_session_key`, and calls
    /// [`TaskSupervisor::mark_failed`] on it. Returns `true` when a
    /// matching task was found and updated; `false` otherwise.
    pub fn mark_child_session_failed(&self, child_session_key: &str, error: &str) -> bool {
        for supervisor in self.live_supervisors() {
            for task in supervisor.get_all_tasks() {
                if task.child_session_key.as_deref() == Some(child_session_key) {
                    supervisor.mark_failed(&task.id, error.to_string());
                    return true;
                }
            }
        }
        false
    }
}

fn system_notice_metadata(sender_user_id: Option<&str>) -> serde_json::Value {
    sender_user_id
        .map(|uid| serde_json::json!({ METADATA_SENDER_USER_ID: uid }))
        .unwrap_or_else(|| serde_json::json!({}))
}

async fn dispatch_background_result_to_actor(
    tx: mpsc::Sender<ActorMessage>,
    payload: BackgroundResultPayload,
) -> bool {
    let task_label = payload.task_label.clone();
    let (ack_tx, ack_rx) = oneshot::channel();
    let send_result = tokio::time::timeout(
        BACKGROUND_RESULT_ACK_TIMEOUT,
        tx.send(ActorMessage::BackgroundResult {
            task_label: payload.task_label,
            content: payload.content,
            kind: payload.kind,
            media: payload.media,
            originating_thread_id: payload.originating_thread_id,
            ack: Some(ack_tx),
        }),
    )
    .await;

    match send_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            record_retry("background_result_actor_closed");
            warn!(
                task_label,
                error = %error,
                "failed to enqueue background result into session actor"
            );
            return false;
        }
        Err(_) => {
            record_retry("background_result_enqueue_timeout");
            warn!(
                task_label,
                timeout_ms = BACKGROUND_RESULT_ACK_TIMEOUT.as_millis(),
                "timed out enqueuing background result into session actor"
            );
            return false;
        }
    }

    match tokio::time::timeout(BACKGROUND_RESULT_ACK_TIMEOUT, ack_rx).await {
        Ok(Ok(persisted)) => persisted,
        Ok(Err(_)) => {
            record_retry("background_result_ack_channel_closed");
            warn!(
                task_label,
                "background result actor acknowledgment channel closed"
            );
            false
        }
        Err(_) => {
            record_retry("background_result_ack_timeout");
            warn!(
                task_label,
                timeout_ms = BACKGROUND_RESULT_ACK_TIMEOUT.as_millis(),
                "timed out waiting for background result actor acknowledgment"
            );
            false
        }
    }
}

/// Build the synthetic `[system-internal]` recovery prompt that the
/// session actor enqueues when a `spawn_only` task transitions to
/// `Failed` (M8.9). The prompt frames the failure for the LLM and asks
/// it to offer a path forward — alternatives parsed from the error, or
/// a safer fallback the model can attempt itself.
pub(crate) fn build_recovery_prompt(signal: &octos_agent::SpawnOnlyFailureSignal) -> String {
    let alternatives_block = if signal.suggested_alternatives.is_empty() {
        String::new()
    } else {
        let list = signal
            .suggested_alternatives
            .iter()
            .map(|alt| format!("- {alt}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\nDetected alternatives:\n{list}\n")
    };
    let input_block = if signal.tool_input.is_null() {
        String::new()
    } else {
        let pretty = serde_json::to_string(&signal.tool_input).unwrap_or_else(|_| "{}".into());
        format!("\nOriginal input: {pretty}")
    };
    format!(
        "[system-internal] Your previous `{tool}` call failed.\n\
         Error: {err}{input}{alts}\n\
         Respond to the user with a path forward — offer the alternatives, or try the safest one yourself if appropriate. Do not just report failure.",
        tool = signal.tool_name,
        err = signal.error_message,
        input = input_block,
        alts = alternatives_block,
    )
}

fn git_turn_summary(content: &str) -> String {
    let compact = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        "agent turn update".to_string()
    } else {
        compact
    }
}

fn merge_attachment_prompt_summaries(
    existing: Option<String>,
    incoming: Option<String>,
) -> Option<String> {
    match (existing, incoming) {
        (Some(mut existing), Some(incoming)) => {
            if !incoming.is_empty() {
                if !existing.is_empty() {
                    existing.push_str("\n\n");
                }
                existing.push_str(&incoming);
            }
            Some(existing)
        }
        (Some(existing), None) => Some(existing),
        (None, Some(incoming)) => Some(incoming),
        (None, None) => None,
    }
}

fn merge_optional_text(existing: Option<String>, incoming: Option<String>) -> Option<String> {
    match (existing, incoming) {
        (Some(mut existing), Some(incoming)) => {
            if !incoming.is_empty() {
                if !existing.is_empty() {
                    existing.push_str("\n\n");
                }
                existing.push_str(&incoming);
            }
            Some(existing)
        }
        (Some(existing), None) => Some(existing),
        (None, Some(incoming)) => Some(incoming),
        (None, None) => None,
    }
}

fn topic_requires_serial_delivery(topic: Option<&str>) -> bool {
    topic.is_some_and(|value| value.starts_with("slides"))
        || topic.is_some_and(|value| value == "site" || value.starts_with("site "))
}

async fn snapshot_workspace_turn_for_path(
    session_key: &SessionKey,
    workspace_root: std::path::PathBuf,
    turn_summary: &str,
) -> Option<String> {
    let turn_summary = git_turn_summary(turn_summary);

    match tokio::task::spawn_blocking(move || {
        octos_agent::snapshot_workspace_turn(&workspace_root, &turn_summary)
    })
    .await
    {
        Ok(Ok(report)) => {
            if !report.committed.is_empty() {
                info!(
                    session = %session_key,
                    repos = ?report.committed,
                    "workspace turn snapshot committed"
                );
            }
            if report.enforced_failures.is_empty() && report.validation_failures.is_empty() {
                return None;
            }

            if !report.validation_failures.is_empty() {
                warn!(
                    session = %session_key,
                    failures = ?report.validation_failures,
                    "workspace contract validation failed"
                );
            }

            let enforcement_notice = if report.enforced_failures.is_empty() {
                None
            } else {
                let repo_labels = report
                    .enforced_failures
                    .iter()
                    .map(|failure| failure.repo_label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let first_error = report
                    .enforced_failures
                    .first()
                    .map(|failure| failure.error.as_str())
                    .unwrap_or("unknown error");
                warn!(
                    session = %session_key,
                    failures = ?report.enforced_failures,
                    "workspace turn snapshot enforcement failed"
                );
                Some(format!(
                    "Workspace versioning failed for {repo_labels}. Turn snapshot was not recorded.\nError: {first_error}"
                ))
            };

            let validation_notice = if report.validation_failures.is_empty() {
                None
            } else {
                let failures = report
                    .validation_failures
                    .iter()
                    .map(|failure| {
                        format!(
                            "{} [{}] {}: {}",
                            failure.repo_label,
                            match failure.phase {
                                octos_agent::WorkspaceValidationPhase::TurnEnd => "turn_end",
                                octos_agent::WorkspaceValidationPhase::Completion => "completion",
                            },
                            failure.check,
                            failure.reason
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(format!("Workspace contract validation failed:\n{failures}"))
            };

            merge_optional_text(enforcement_notice, validation_notice)
        }
        Ok(Err(error)) => {
            warn!(
                session = %session_key,
                error = %error,
                "workspace turn snapshot failed"
            );
            Some(format!(
                "Workspace versioning failed. Turn snapshot was not recorded.\nError: {error}"
            ))
        }
        Err(error) => {
            warn!(
                session = %session_key,
                error = %error,
                "workspace turn snapshot task failed"
            );
            Some(format!(
                "Workspace versioning task failed. Turn snapshot was not recorded.\nError: {error}"
            ))
        }
    }
}

async fn emit_workspace_snapshot_notice(
    out_tx: &mpsc::Sender<OutboundMessage>,
    channel: &str,
    chat_id: &str,
    reply_to: Option<String>,
    sender_user_id: Option<&str>,
    content: String,
) {
    let _ = out_tx
        .send(OutboundMessage {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            content,
            reply_to,
            media: vec![],
            metadata: system_notice_metadata(sender_user_id),
        })
        .await;
}

// ── Messages ────────────────────────────────────────────────────────────────

/// Messages dispatched to a session actor.
pub enum ActorMessage {
    /// A user message to process.
    Inbound {
        message: InboundMessage,
        image_media: Vec<String>,
        attachment_media: Vec<String>,
        attachment_prompt: Option<String>,
    },
    /// Result from a background subagent task — injected as a system message
    /// into the conversation without triggering an extra LLM call.
    BackgroundResult {
        /// Task identifier for attribution.
        task_label: String,
        /// The subagent's final output.
        content: String,
        /// Delivery semantics for this result.
        kind: BackgroundResultKind,
        /// Media files attached to this terminal background result.
        media: Vec<String>,
        /// M8.10 follow-up (#649): the user message's `client_message_id`
        /// from the turn that originated this background task. Stamped onto
        /// the outbound's `metadata.thread_id` so wire-side SSE events land
        /// under the originating bubble even after subsequent unrelated user
        /// turns have rotated the per-chat sticky thread_id. `None` for
        /// legacy callers and tests that pre-date #649.
        originating_thread_id: Option<String>,
        /// Completion acknowledgment for durable persistence.
        ack: Option<oneshot::Sender<bool>>,
    },
    /// Background task status changed — push to SSE.
    TaskStatusChanged {
        /// Serialized JSON of the BackgroundTask.
        task_json: String,
    },
    /// Synthetic recovery turn enqueued by the spawn_only failure-signal
    /// callback (M8.9). Drives the LLM to re-engage on a failed background
    /// task with the actionable error and (optionally parsed) alternatives.
    RecoveryHint {
        /// Task ID that triggered the recovery (used for de-duplication).
        task_id: String,
        /// Tool that failed — surfaced verbatim in the synthetic prompt.
        tool_name: String,
        /// Best-effort prompt body. Already framed as `[system-internal]` so
        /// the LLM treats it as runtime guidance, not a user turn.
        prompt: String,
        /// Issue #738 fix: the originating user turn's `client_message_id`,
        /// captured when the spawn_only task was registered. Stamped onto
        /// the synthetic recovery `InboundMessage`'s metadata so
        /// `process_inbound` reuses it as the cmid for the recovery turn
        /// instead of minting a fresh server UUIDv7. `None` for legacy
        /// callers / tests that pre-date the fix.
        originating_client_message_id: Option<String>,
    },
    /// Cancel the current operation.
    Cancel,
}

// ── ActorHandle ─────────────────────────────────────────────────────────────

/// Handle to a running session actor.
pub struct ActorHandle {
    pub tx: mpsc::Sender<ActorMessage>,
    pub created_at: Instant,
    join_handle: JoinHandle<()>,
    /// Profile system prompt override — preserved for respawn on actor death.
    system_prompt_override: Option<String>,
    /// Sender user ID for outbound identity assertion — preserved for respawn.
    sender_user_id: Option<String>,
    /// Profile-specific factory cache key for respawn after actor death.
    factory_profile_id: Option<String>,
}

impl ActorHandle {
    /// Whether the actor task has completed (idle-timeout, panic, etc.).
    pub fn is_finished(&self) -> bool {
        self.join_handle.is_finished()
    }
}

// ── ActorRegistry ───────────────────────────────────────────────────────────

/// Manages the lifecycle of session actors.
pub struct ActorRegistry {
    actors: HashMap<String, ActorHandle>,
    factory: Arc<ActorFactory>,
    profile_factories: HashMap<String, Arc<ActorFactory>>,
    semaphore: Arc<Semaphore>,
    out_tx: mpsc::Sender<OutboundMessage>,
    pending_messages: PendingMessages,
}

impl ActorRegistry {
    pub fn new(
        factory: ActorFactory,
        semaphore: Arc<Semaphore>,
        out_tx: mpsc::Sender<OutboundMessage>,
        pending_messages: PendingMessages,
    ) -> Self {
        Self {
            actors: HashMap::new(),
            factory: Arc::new(factory),
            profile_factories: HashMap::new(),
            semaphore,
            out_tx,
            pending_messages,
        }
    }

    pub fn register_profile_factory(
        &mut self,
        profile_id: impl Into<String>,
        factory: ActorFactory,
    ) {
        self.profile_factories
            .insert(profile_id.into(), Arc::new(factory));
    }

    pub fn has_profile_factory(&self, profile_id: &str) -> bool {
        self.profile_factories.contains_key(profile_id)
    }

    fn actor_key(session_key: &SessionKey, profile_id: Option<&str>) -> String {
        if session_key.profile_id().is_some() {
            session_key.to_string()
        } else {
            format!("{}:{}", profile_id.unwrap_or(MAIN_PROFILE_ID), session_key)
        }
    }

    fn resolve_factory(&self, profile_id: Option<&str>) -> (Arc<ActorFactory>, Option<String>) {
        if let Some(profile_id) = profile_id {
            if let Some(factory) = self.profile_factories.get(profile_id) {
                return (factory.clone(), Some(profile_id.to_string()));
            }
        }
        (self.factory.clone(), None)
    }

    /// Route an inbound message to the correct actor, creating one if needed.
    pub async fn dispatch(&mut self, params: DispatchParams<'_>) {
        let DispatchParams {
            message,
            image_media,
            attachment_media,
            attachment_prompt,
            session_key,
            reply_channel,
            reply_chat_id,
            status_indicator,
            profile_id,
            system_prompt_override,
            sender_user_id,
        } = params;
        let key_str = Self::actor_key(&session_key, profile_id);

        // If actor exists but has finished (idle-timeout/panic), remove it
        if let Some(handle) = self.actors.get(&key_str) {
            if handle.is_finished() {
                self.actors.remove(&key_str);
            }
        }

        // Create actor if needed
        if !self.actors.contains_key(&key_str) {
            let (factory, factory_profile_id) = self.resolve_factory(profile_id);
            let (tx, join_handle) = factory.spawn(SpawnParams {
                session_key: session_key.clone(),
                channel: reply_channel,
                chat_id: reply_chat_id,
                semaphore: self.semaphore.clone(),
                status_indicator: status_indicator.clone(),
                system_prompt_override: system_prompt_override.clone(),
                sender_user_id: sender_user_id.clone(),
            });
            self.actors.insert(
                key_str.clone(),
                ActorHandle {
                    tx,
                    created_at: Instant::now(),
                    join_handle,
                    system_prompt_override,
                    sender_user_id: sender_user_id.clone(),
                    factory_profile_id,
                },
            );
        }

        let handle = self.actors.get(&key_str).unwrap();
        let actor_msg = ActorMessage::Inbound {
            message,
            image_media,
            attachment_media,
            attachment_prompt,
        };

        match handle.tx.try_send(actor_msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(actor_msg)) => {
                // Actor inbox is full — send backpressure feedback
                let _ = self
                    .out_tx
                    .send(OutboundMessage {
                        channel: reply_channel.to_string(),
                        chat_id: reply_chat_id.to_string(),
                        content: "⏳ Still processing, your message is queued...".to_string(),
                        reply_to: None,
                        media: vec![],
                        metadata: system_notice_metadata(sender_user_id.as_deref()),
                    })
                    .await;
                // Now block until space is available
                let handle = self.actors.get(&key_str).unwrap();
                let _ = handle.tx.send(actor_msg).await;
            }
            Err(mpsc::error::TrySendError::Closed(actor_msg)) => {
                // Actor died — retrieve profile overrides, then respawn
                let dead = self.actors.remove(&key_str);
                let (prompt_override, uid_override, factory_profile_id) = dead
                    .map(|h| {
                        (
                            h.system_prompt_override,
                            h.sender_user_id,
                            h.factory_profile_id,
                        )
                    })
                    .unwrap_or((None, None, None));
                let factory = factory_profile_id
                    .as_deref()
                    .and_then(|pid| self.profile_factories.get(pid))
                    .cloned()
                    .unwrap_or_else(|| self.factory.clone());
                let (tx, join_handle) = factory.spawn(SpawnParams {
                    session_key,
                    channel: reply_channel,
                    chat_id: reply_chat_id,
                    semaphore: self.semaphore.clone(),
                    status_indicator,
                    system_prompt_override: prompt_override.clone(),
                    sender_user_id: uid_override.clone(),
                });
                let _ = tx.send(actor_msg).await;
                self.actors.insert(
                    key_str,
                    ActorHandle {
                        tx,
                        created_at: Instant::now(),
                        join_handle,
                        system_prompt_override: prompt_override,
                        sender_user_id: uid_override,
                        factory_profile_id,
                    },
                );
            }
        }
    }

    /// Returns the dispatch keys of all active actors (for testing).
    #[cfg(test)]
    pub fn actor_keys(&self) -> Vec<String> {
        self.actors.keys().cloned().collect()
    }

    /// Remove actors whose tasks have completed.
    pub fn reap_dead_actors(&mut self) {
        self.actors.retain(|key, handle| {
            if handle.is_finished() {
                debug!(session = %key, "reaping completed actor");
                false
            } else {
                true
            }
        });
    }

    /// Stop and remove a session actor. Drops the sender so the actor's run
    /// loop exits on the next recv(). Used when a session is deleted — the
    /// actor must not survive and serve stale context to new messages.
    pub fn remove_session(&mut self, session_key: &str) {
        let scoped_suffix = format!(":{session_key}");
        let keys_to_remove: Vec<String> = self
            .actors
            .keys()
            .filter(|key| key.as_str() == session_key || key.ends_with(&scoped_suffix))
            .cloned()
            .collect();
        for key in keys_to_remove {
            if let Some(handle) = self.actors.remove(&key) {
                debug!(session = %key, "removing session actor on delete");
                drop(handle.tx); // actor's recv() returns None → run loop exits
            }
        }
    }

    /// Cancel a specific session actor.
    pub async fn cancel(&self, session_key: &str) {
        let scoped_suffix = format!(":{session_key}");
        let handles: Vec<_> = self
            .actors
            .iter()
            .filter(|(key, _)| key.as_str() == session_key || key.ends_with(&scoped_suffix))
            .map(|(_, handle)| handle.tx.clone())
            .collect();
        for tx in handles {
            let _ = tx.send(ActorMessage::Cancel).await;
        }
    }

    /// Shut down all actors gracefully.
    pub async fn shutdown_all(self) {
        // Drop all senders — actors will exit on recv() returning None
        let handles: Vec<_> = self
            .actors
            .into_values()
            .map(|h| {
                drop(h.tx);
                h.join_handle
            })
            .collect();

        for h in handles {
            let _ = h.await;
        }
    }

    /// Flush buffered messages for a session key (called on `/s` switch).
    /// Returns the number of messages flushed.
    pub async fn flush_pending(&self, session_key: &str) -> usize {
        let messages = self
            .pending_messages
            .lock()
            .await
            .remove(session_key)
            .unwrap_or_default();
        let count = messages.len();
        for msg in messages {
            let _ = self.out_tx.send(msg).await;
        }
        count
    }

    /// Number of active actors.
    pub fn len(&self) -> usize {
        self.actors.len()
    }

    /// Whether there are no active actors.
    pub fn is_empty(&self) -> bool {
        self.actors.is_empty()
    }
}

// ── ActorFactory ────────────────────────────────────────────────────────────

/// Shared resources needed to create per-session actors.
pub struct ActorFactory {
    pub agent_config: AgentConfig,
    pub llm: Arc<dyn LlmProvider>,
    pub llm_for_compaction: Arc<dyn LlmProvider>,
    /// Strong-only provider chain for slides sessions (kimi + deepseek + minimax).
    pub llm_strong: Arc<dyn LlmProvider>,
    pub memory: Arc<EpisodeStore>,
    pub system_prompt: Arc<std::sync::RwLock<String>>,
    pub hooks: Option<Arc<HookExecutor>>,
    pub hook_context_template: Option<HookContext>,
    /// Data directory for creating per-actor SessionHandle instances.
    pub data_dir: std::path::PathBuf,
    /// Shared SessionManager for admin operations (/sessions, /new, /delete).
    /// NOT used by actors — only by the gateway main loop.
    pub session_mgr: Arc<Mutex<SessionManager>>,
    pub out_tx: mpsc::Sender<OutboundMessage>,
    pub spawn_inbound_tx: mpsc::Sender<InboundMessage>,
    pub cron_service: Option<Arc<octos_bus::CronService>>,
    pub tool_registry_factory: Arc<dyn ToolRegistryFactory + Send + Sync>,
    pub pipeline_factory: Option<Arc<dyn PipelineToolFactory + Send + Sync>>,
    pub max_history: Arc<std::sync::atomic::AtomicUsize>,
    pub idle_timeout: Duration,
    pub session_timeout: Duration,
    pub shutdown: Arc<AtomicBool>,
    /// Working directory for SpawnTool (shared profile-level cwd).
    pub cwd: std::path::PathBuf,
    /// Sandbox config — used to create per-user sandbox instances.
    pub sandbox_config: octos_agent::SandboxConfig,
    /// Provider policy for SpawnTool and PipelineTool.
    pub provider_policy: Option<ToolPolicy>,
    /// Global `tool_policy` from config. The base registry has this applied
    /// at construction time, but per-session tools (notably `run_pipeline`)
    /// are registered later by [`ActorFactory::spawn`]. Re-applying after
    /// per-session registration ensures globally denied tools cannot slip
    /// in through the per-session registration path. See PR #688 follow-up
    /// (MEDIUM #4): `gateway_runtime.rs` calls `apply_policy` BEFORE the
    /// `ActorFactory` adds `run_pipeline`, so without this re-application
    /// the global deny is bypassed for spawn_only-marked tools.
    pub tool_policy: Option<ToolPolicy>,
    /// Worker system prompt for SpawnTool subagents.
    pub worker_prompt: Option<String>,
    /// Provider router for SpawnTool and PipelineTool.
    pub provider_router: Option<Arc<ProviderRouter>>,
    /// Optional embedder for episodic memory recall.
    pub embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// Active session store — used to check if a session is currently active.
    pub active_sessions: Arc<RwLock<ActiveSessionStore>>,
    /// Pending message buffer — replies from inactive sessions are held here.
    pub pending_messages: PendingMessages,
    /// Queue mode for handling messages arriving during active agent runs.
    pub queue_mode: QueueMode,
    /// Side-channel to the AdaptiveRouter for responsiveness feedback.
    /// None when adaptive routing is disabled or using a static provider chain.
    pub adaptive_router: Option<Arc<AdaptiveRouter>>,
    /// Memory store for saving long-form outputs (research reports) to the
    /// memory bank so only a summary is injected into session context.
    pub memory_store: Option<Arc<MemoryStore>>,
    /// Plugin directories for SpawnTool subagents to load plugin tools.
    pub plugin_dirs: Vec<std::path::PathBuf>,
    /// Extra environment variables for plugin processes in subagents.
    pub plugin_extra_env: Vec<(String, String)>,
    /// Session-scoped background task lookup for API inspection.
    pub task_query_store: SessionTaskQueryStore,
    /// M8 fix-first item 8 (gap 2): shared SubAgentOutputRouter — one
    /// router instance backs every actor so dashboards see a consistent
    /// disk layout across sessions. Built once at factory construction
    /// time and cloned (cheap Arc bump) per actor.
    pub subagent_output_router: Arc<octos_agent::SubAgentOutputRouter>,
}

/// Trait for creating per-session ToolRegistry instances.
///
/// This abstracts the complex tool registration logic (builtins, plugins, MCP,
/// policies, etc.) so the actor module doesn't depend on all those details.
pub trait ToolRegistryFactory: Send + Sync {
    /// Create a base ToolRegistry with all non-session-specific tools registered.
    /// The caller will add session-specific tools (MessageTool, SendFileTool, etc.)
    fn create_base_registry(&self) -> ToolRegistry;

    /// Create a base ToolRegistry with cwd-bound tools re-bound to a per-user
    /// workspace directory. Non-cwd tools (web, MCP, plugins) are preserved.
    /// The sandbox is created fresh for the per-user workspace path.
    fn create_registry_for_workspace(
        &self,
        workspace: &std::path::Path,
        sandbox: Box<dyn octos_agent::Sandbox>,
    ) -> ToolRegistry;
}

/// Trait for creating per-session pipeline tool instances.
pub trait PipelineToolFactory: Send + Sync {
    fn create(&self) -> Arc<dyn octos_agent::tools::Tool>;
}

/// ToolRegistryFactory backed by snapshot_excluding() — clones shared tools cheaply.
pub struct SnapshotToolRegistryFactory {
    base: ToolRegistry,
}

impl SnapshotToolRegistryFactory {
    pub fn new(base: ToolRegistry) -> Self {
        Self { base }
    }
}

impl ToolRegistryFactory for SnapshotToolRegistryFactory {
    fn create_base_registry(&self) -> ToolRegistry {
        // Clone all tools (Arc refcount bumps, cheap)
        self.base.snapshot_excluding(&[])
    }

    fn create_registry_for_workspace(
        &self,
        workspace: &std::path::Path,
        sandbox: Box<dyn octos_agent::Sandbox>,
    ) -> ToolRegistry {
        // Re-bind cwd-bound tools to the per-user workspace while
        // preserving non-cwd tools (web_search, browser, MCP, plugins, etc.)
        self.base.rebind_cwd(workspace, sandbox)
    }
}

impl ActorFactory {
    /// Spawn a new session actor, returning its inbox sender and join handle.
    fn spawn(&self, params: SpawnParams<'_>) -> (mpsc::Sender<ActorMessage>, JoinHandle<()>) {
        let SpawnParams {
            session_key,
            channel,
            chat_id,
            semaphore,
            status_indicator,
            system_prompt_override,
            sender_user_id,
        } = params;
        let (tx, rx) = mpsc::channel(ACTOR_INBOX_SIZE);

        // Create a per-session proxy channel. ALL outbound messages from this
        // session (tools, final reply, errors) flow through proxy_tx. A
        // forwarding task checks whether this session is active and either
        // delivers immediately or buffers for later.
        let (proxy_tx, proxy_rx) = mpsc::channel::<OutboundMessage>(64);

        // Per-session tools — they write to proxy_tx, not the real out_tx
        let message_tool = MessageTool::with_context(proxy_tx.clone(), channel, chat_id);

        // Build per-user workspace directory for file isolation.
        // Each user's tools are restricted to their own workspace via
        // resolve_path() (application-level) and sandbox-exec SBPL (kernel-level on macOS).
        let encoded_base = octos_bus::session::encode_path_component(session_key.base_key());
        let user_workspace = self
            .data_dir
            .join("users")
            .join(&encoded_base)
            .join("workspace");
        // Create the per-actor session handle early so we can derive the
        // background task ledger path before any worker can mutate state.
        let mut session_handle = SessionHandle::open(&self.data_dir, &session_key);
        // M8 fix-first item 8 (gap 1): construct the per-actor
        // FileStateCache BEFORE sanitize so the resume hand-off can seed
        // it directly. The same Arc is later wired into Agent::new so the
        // recovered file-identity claims actually reach the file tools.
        let file_state_cache = Arc::new(octos_agent::FileStateCache::new());
        // M8.6: sanitize the loaded transcript. Dropping unresolved tool
        // calls, orphan thinking, and whitespace-only messages here
        // prevents the provider from 400-ing on the first request after a
        // resume. Pass the user_workspace so the sanitizer can detect a
        // missing-on-disk workspace and hard-refuse — must run BEFORE
        // create_dir_all below, otherwise the recreate would mask the
        // missing-workspace condition we want to catch.
        //
        // Skip the worktree check entirely when there is no loaded
        // transcript: every brand-new session (including pipeline workers
        // and spawn_only children) hits this path before its workspace
        // dir is materialised, and firing WorktreeMissing on a fresh
        // session causes the is_child branch below to falsely
        // mark_child_session_failed on the parent task — breaking
        // run_pipeline. The check is only meaningful when there are
        // messages whose tool-result references could be invalidated by a
        // missing worktree.
        let has_loaded_messages = !session_handle.get_history(1).is_empty();
        let workspace_root_for_sanitize: Option<&Path> = if has_loaded_messages {
            Some(&user_workspace)
        } else {
            None
        };
        match session_handle.sanitize_loaded_messages(None, workspace_root_for_sanitize) {
            Ok((report, refs)) => {
                if report.input_len != report.output_len
                    || report.content_replacements_restored > 0
                    || !report.warnings.is_empty()
                {
                    info!(
                        session = %session_key,
                        report = %report,
                        "resume sanitize applied"
                    );
                }
                // M8.4/M8.6 fix-first item 7 + M8 fix-first item 8 (gap 1):
                // hand-off complete and now WIRED. Seed the per-actor
                // FileStateCache with the recovered replacement refs
                // before any LLM call so post-resume reads consult the
                // recovered hashes instead of returning false
                // FILE_UNCHANGED stubs.
                let seeded = file_state_cache.seed_from_replacement_refs(&refs);
                if seeded > 0 {
                    info!(
                        session = %session_key,
                        seeded,
                        "seeded FileStateCache from resume refs"
                    );
                }
            }
            Err(error) => {
                // M8.6 fix-first item 3: a refused sanitize means the
                // worktree is gone and the loaded transcript references
                // state we cannot trust. The legacy "warn and continue"
                // path silently fed the unsafe transcript into the first
                // LLM call. We now hard-refuse:
                //
                // - top-level sessions: drop the in-memory transcript so
                //   the actor restarts with an empty session. The disk
                //   JSONL is left untouched so an operator can recover
                //   it; only the in-memory copy is cleared.
                // - child / background sessions: M8 fix-first item 8
                //   (gap 3) — mark the owning parent task as failed via
                //   the supervisor lookup so dashboards see the cascade
                //   instead of a stuck Running entry. The transcript
                //   clear stays as the safety floor underneath.
                let is_child = session_handle.is_child_session();
                session_handle.clear_messages_for_unsafe_resume();
                let octos_bus::SanitizeError::WorktreeMissing { path, .. } = &error;
                let mut parent_marked_failed = false;
                if is_child {
                    let failure_reason = format!(
                        "resume sanitize refused: worktree missing at {}",
                        path.display()
                    );
                    parent_marked_failed = self
                        .task_query_store
                        .mark_child_session_failed(&session_key.to_string(), &failure_reason);
                }
                warn!(
                    session = %session_key,
                    path = %path.display(),
                    is_child,
                    parent_marked_failed,
                    "resume sanitize HARD-REFUSED: worktree missing — \
                     in-memory transcript dropped to prevent unsafe LLM call"
                );
            }
        }
        // Recreate the per-user workspace AFTER sanitize so the resume
        // refusal above had a chance to detect the missing-on-disk state.
        if let Err(e) = std::fs::create_dir_all(&user_workspace) {
            warn!(
                session = %session_key,
                path = %user_workspace.display(),
                "failed to create per-user workspace: {e}, falling back to shared cwd"
            );
        }
        let task_state_path = session_handle.task_state_path();
        let session_handle = Arc::new(Mutex::new(session_handle));
        let session_policy_path = workspace_policy_path(&user_workspace);
        let desired_session_policy = WorkspacePolicy::for_session();
        let active_workspace_policy: Option<WorkspacePolicy> =
            match read_workspace_policy(&user_workspace) {
                Ok(Some(mut existing_policy)) => {
                    let mut updated = false;
                    for (name, pattern) in &desired_session_policy.artifacts.entries {
                        if !existing_policy.artifacts.entries.contains_key(name) {
                            existing_policy
                                .artifacts
                                .entries
                                .insert(name.clone(), pattern.clone());
                            updated = true;
                        }
                    }
                    for (name, task) in &desired_session_policy.spawn_tasks {
                        if !existing_policy.spawn_tasks.contains_key(name) {
                            existing_policy
                                .spawn_tasks
                                .insert(name.clone(), task.clone());
                            updated = true;
                        }
                    }
                    if updated {
                        if let Err(error) =
                            write_workspace_policy(&user_workspace, &existing_policy)
                        {
                            warn!(
                                session = %session_key,
                                path = %session_policy_path.display(),
                                "failed to upgrade session workspace policy: {error}"
                            );
                        }
                    }
                    Some(existing_policy)
                }
                Ok(None) => {
                    if let Err(error) =
                        write_workspace_policy(&user_workspace, &desired_session_policy)
                    {
                        warn!(
                            session = %session_key,
                            path = %session_policy_path.display(),
                            "failed to write session workspace policy: {error}"
                        );
                    }
                    Some(desired_session_policy.clone())
                }
                Err(error) => {
                    warn!(
                        session = %session_key,
                        path = %session_policy_path.display(),
                        "failed to read session workspace policy: {error}"
                    );
                    None
                }
            };

        // send_file resolves relative paths against user_workspace (same as
        // write_file/read_file) so the LLM can write+send in one flow.
        // data_dir is an extra allowed directory for pipeline-generated files.
        let send_file_tool = SendFileTool::with_context(proxy_tx.clone(), channel, chat_id)
            .with_topic(session_key.topic().map(str::to_string))
            .with_base_dir(&user_workspace)
            .with_extra_allowed_dir(&self.data_dir);
        let session_hook_context = self.hook_context_template.as_ref().map(|ctx| HookContext {
            session_id: Some(session_key.to_string()),
            profile_id: ctx.profile_id.clone(),
        });

        // Create tool registry with cwd-bound tools pointing to the per-user workspace.
        // A fresh sandbox is created per user so the SBPL profile restricts writes
        // to this user's workspace directory (kernel-enforced on macOS).
        let user_sandbox = octos_agent::create_sandbox(&self.sandbox_config);
        let mut tools = self
            .tool_registry_factory
            .create_registry_for_workspace(&user_workspace, user_sandbox);
        let supervisor = tools.supervisor();
        if let Err(error) = supervisor.enable_persistence(&task_state_path) {
            warn!(
                session = %session_key,
                error = %error,
                "failed to enable task supervisor persistence"
            );
        }
        self.task_query_store
            .register(&session_key, &supervisor, &self.data_dir);
        tools.rebind_plugin_work_dirs(&user_workspace);
        tools.set_session_key(session_key.to_string());
        tools.register(CheckBackgroundTasksTool::new(
            supervisor.clone(),
            session_key.to_string(),
        ));
        tools.register(message_tool);
        tools.register(send_file_tool);

        // M8 Runtime Parity W2.B1: build the same M8.7 summary generator
        // that goes onto the parent Agent so the child workers we spawn
        // observe an identical contract. (The Agent::new wiring further
        // down also consumes this Arc — keep them in sync.)
        let subagent_summary_generator_for_spawn =
            Arc::new(octos_agent::AgentSummaryGenerator::new(
                self.llm_for_compaction.clone(),
                self.subagent_output_router.clone(),
                (*supervisor).clone(),
            ));

        // Spawn tool (per-session context, fully configured)
        let mut spawn_tool = SpawnTool::with_context(
            self.llm.clone(),
            self.memory.clone(),
            self.cwd.clone(),
            self.spawn_inbound_tx.clone(),
            channel,
            chat_id,
        )
        .with_provider_policy(self.provider_policy.clone())
        .with_agent_config(self.agent_config.clone())
        .with_task_supervisor(
            supervisor.clone(),
            session_key.to_string(),
            task_state_path.clone(),
        )
        // M8 Runtime Parity W2.B1: parent → child cache inheritance.
        // Without these the spawned child Agent observes
        // `file_state_cache: None` and `subagent_output_router: None`
        // and the post-M8.4 / M8.7 contracts are silently bypassed.
        .with_parent_file_state_cache(file_state_cache.clone())
        .with_parent_subagent_output_router(self.subagent_output_router.clone())
        .with_parent_subagent_summary_generator(subagent_summary_generator_for_spawn);
        if let Some(ref prompt) = self.worker_prompt {
            spawn_tool = spawn_tool.with_worker_prompt(prompt.clone());
        }
        if let Some(ref router) = self.provider_router {
            spawn_tool = spawn_tool.with_provider_router(router.clone());
        }
        if !self.plugin_dirs.is_empty() {
            spawn_tool = spawn_tool
                .with_plugin_dirs(self.plugin_dirs.clone(), self.plugin_extra_env.clone());
        }
        if let Some(ref hooks) = self.hooks {
            spawn_tool = spawn_tool.with_hooks(hooks.clone());
        }
        if let Some(ref ctx) = session_hook_context {
            spawn_tool = spawn_tool.with_hook_context(ctx.clone());
        }
        if let Some(ref pipeline_factory) = self.pipeline_factory {
            let pipeline_factory = pipeline_factory.clone();
            spawn_tool =
                spawn_tool.with_child_tool_factory(Arc::new(move || pipeline_factory.create()));
        }

        // Wire direct background result injection (bypasses InboundMessage relay)
        let bg_tx = tx.clone();
        spawn_tool = spawn_tool.with_background_result_sender(Arc::new(
            move |payload: BackgroundResultPayload| {
                let tx = bg_tx.clone();
                Box::pin(async move { dispatch_background_result_to_actor(tx, payload).await })
            },
        ));

        let child_data_dir = self.data_dir.clone();
        spawn_tool = spawn_tool.with_child_session_sender(Arc::new(
            move |payload: ChildSessionLifecyclePayload| {
                let child_data_dir = child_data_dir.clone();
                Box::pin(async move {
                    match persist_child_session_lifecycle(&child_data_dir, &payload).await {
                        Ok(joined) => joined,
                        Err(error) => {
                            record_child_session_lifecycle(payload.kind, "persist_failed");
                            warn!(
                                parent_session = %payload.parent_session_key,
                                child_session = %payload.child_session_key,
                                error = %error,
                                "failed to persist child-session lifecycle event"
                            );
                            false
                        }
                    }
                })
            },
        ));

        tools.register(spawn_tool);

        // Wire background result sender for spawn_only tool lifecycle notifications
        let bg_tx2 = tx.clone();
        tools.set_background_result_sender(Arc::new(move |payload: BackgroundResultPayload| {
            let tx = bg_tx2.clone();
            Box::pin(async move { dispatch_background_result_to_actor(tx, payload).await })
        }));

        // Wire supervisor on_change callback to push task status via SSE.
        // M9-06: terminal lifecycle states (Completed/Failed/Cancelled) MUST
        // NOT be silently dropped under inbox backpressure (32 slots), or the
        // UI / SSE consumers stay stuck on `running`. See
        // [`forward_task_status_to_actor_inbox`].
        let status_tx = tx.clone();
        let task_data_dir = self.data_dir.clone();
        supervisor.set_on_change(move |task| {
            forward_task_status_to_actor_inbox(&status_tx, &task_data_dir, task);
        });

        // Wire supervisor on_failure_signal callback (M8.9): when a
        // spawn_only task transitions to Failed, enqueue a synthetic
        // recovery turn so the LLM can offer alternatives or take a
        // recovery action instead of leaving the user with only a
        // terminal failure notification.
        let recovery_tx = tx.clone();
        supervisor.set_on_failure_signal(move |signal| {
            let prompt = build_recovery_prompt(signal);
            let _ = recovery_tx.try_send(ActorMessage::RecoveryHint {
                task_id: signal.task_id.clone(),
                tool_name: signal.tool_name.clone(),
                prompt,
                // Issue #738 fix: forward the originating user turn's
                // cmid into the recovery hint so the synthetic
                // InboundMessage stamps `client_message_id` into its
                // metadata and `process_inbound` reuses it instead of
                // minting an orphan UUIDv7.
                originating_client_message_id: signal.originating_client_message_id.clone(),
            });
        });

        let cron_tool_ref = if let Some(ref cron_service) = self.cron_service {
            let cron_tool = Arc::new(CronTool::with_context(
                cron_service.clone(),
                channel,
                chat_id,
            ));
            tools.register_arc(cron_tool.clone());
            Some(cron_tool)
        } else {
            None
        };

        if let Some(ref pf) = self.pipeline_factory {
            let pt = pf.create();
            tools.register_arc(pt);
            tools.mark_spawn_only(
                "run_pipeline",
                Some(
                    "Pipeline started in background. The final result and any artifacts will be sent here when complete. You can keep chatting in the meantime."
                        .to_string(),
                ),
            );
        }

        // PR #688 follow-up — MEDIUM #4: re-apply the global tool_policy
        // AFTER the per-session pipeline tool was registered. The base
        // registry already had `apply_policy` invoked during construction
        // (in `gateway_runtime.rs`), but `run_pipeline` is only registered
        // here at session-spawn time. Without this second pass, a config
        // `tool_policy.deny: ["run_pipeline"]` is silently ignored on
        // gateway-spawned actors. Mirrors the chat.rs pattern that already
        // applies policy AFTER registering the pipeline tool.
        if let Some(ref policy) = self.tool_policy {
            tools.apply_policy(policy);
        }

        // Defer rarely-used per-session tools to keep active tool count low
        // for providers that choke on many tools (e.g. Dashscope).
        // Keep cron active so reminder flows don't require activate_tools.
        tools.defer(["spawn".to_string()]);

        // For slides sessions, auto-activate media tools and use primary model
        // (bypasses adaptive router which may pick a weak model).
        let is_slides = session_key.topic().is_some_and(|t| t.starts_with("slides"));
        let is_site = session_key
            .topic()
            .is_some_and(|t| t == "site" || t.starts_with("site "));
        if is_slides {
            tools.activate("group:media");

            // Scaffold slides project INTO the workspace so file tools
            // (read_file, write_file, mofa_slides) all resolve the same paths.
            // The earlier scaffold in gateway_dispatcher writes to data_dir
            // which is unreachable from the sandboxed workspace.
            let topic = session_key.topic().unwrap_or("slides");
            let project_name = topic.strip_prefix("slides").unwrap_or("").trim();
            let project_name = if project_name.is_empty() {
                "untitled"
            } else {
                project_name
            };
            if let Err(error) =
                crate::project_templates::scaffold_slides_project(&user_workspace, project_name)
            {
                warn!(session = %session_key, "slides scaffold failed in workspace: {error}");
            }

            // Copy built-in style templates into workspace/styles/ so the
            // agent's glob("styles/*.toml") can discover them.
            let builtin_styles = resolve_builtin_slides_styles_dir(&self.data_dir);
            let ws_styles = user_workspace.join("styles");
            if let Some(builtin_styles) = builtin_styles {
                std::fs::create_dir_all(&ws_styles).ok();
                if let Ok(entries) = std::fs::read_dir(&builtin_styles) {
                    for entry in entries.flatten() {
                        let src = entry.path();
                        if src.extension().is_some_and(|e| e == "toml") {
                            let dst = ws_styles.join(entry.file_name());
                            // Don't overwrite custom styles the user created
                            if !dst.exists() {
                                std::fs::copy(&src, &dst).ok();
                            }
                        }
                    }
                }
                let cyberpunk_alias = ws_styles.join("cyberpunk-neon.toml");
                let blade_runner = ws_styles.join("nb-br.toml");
                if !cyberpunk_alias.exists() && blade_runner.is_file() {
                    std::fs::copy(&blade_runner, &cyberpunk_alias).ok();
                }
            } else {
                warn!(
                    session = %session_key,
                    data_dir = %self.data_dir.display(),
                    "builtin mofa-slides styles directory not found"
                );
            }
        }
        let slides_generation_available = !is_slides || tools.get("mofa_slides").is_some();

        if is_site {
            let topic = session_key.topic().unwrap_or("site");
            let profile_id = session_key.profile_id().unwrap_or(MAIN_PROFILE_ID);
            if let Err(error) = crate::project_templates::scaffold_site_project(
                &user_workspace,
                profile_id,
                session_key.chat_id(),
                topic,
                &self.data_dir,
            ) {
                warn!(session = %session_key, "site scaffold failed in workspace: {error}");
            }
        }

        // Slides sessions use the strong-only provider chain — failover
        // between kimi/deepseek/minimax only, excluding weak providers that
        // hang on 30+ tools. Normal sessions use the full adaptive router.
        let session_llm = if is_slides {
            self.llm_strong.clone()
        } else {
            self.llm.clone()
        };
        let agent_id = AgentId::new(format!("session-{}", session_key));
        let has_deferred = tools.has_deferred();
        let mut system_prompt = system_prompt_override.unwrap_or_else(|| {
            self.system_prompt
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        if is_slides && !slides_generation_available {
            system_prompt.push_str(
                "\n\n## Slides Generation Availability\n\n\
                 `mofa_slides` is not available on this host. You may still design and edit slide projects, \
                 but you must tell the user that PPTX/image generation is unavailable here. \
                 Do NOT retry generation via shell, run_pipeline, or alternative binaries.",
            );
        }
        if has_deferred {
            let groups = tools.deferred_groups();
            let mut tool_names = Vec::new();
            for (name, _desc, _count) in &groups {
                if let Some(info) = octos_agent::tools::policy::TOOL_GROUPS
                    .iter()
                    .find(|g| g.name == name)
                {
                    tool_names.extend(info.tools.iter().copied());
                }
            }
            let template = include_str!("../../octos-agent/src/prompts/deferred_tools.txt");
            system_prompt.push_str(&template.replace("{tool_list}", &tool_names.join(", ")));
        }

        // M8 fix-first item 8 (gap 2): build a per-actor
        // AgentSummaryGenerator now that the supervisor handle and the
        // shared SubAgentOutputRouter are both in scope. The generator
        // binds the per-registry supervisor (so it can mark_terminal /
        // start a watcher / etc. for THIS actor's tasks); the router
        // is shared across actors via the factory.
        //
        // M8 Runtime Parity W2.B1: the same Arc is also threaded onto
        // the SpawnTool via `with_parent_subagent_summary_generator`
        // (above) so child agents observe the same generator the
        // parent does.
        let subagent_summary_generator = Arc::new(octos_agent::AgentSummaryGenerator::new(
            self.llm_for_compaction.clone(),
            self.subagent_output_router.clone(),
            (*supervisor).clone(),
        ));

        // Per-session cancellation flag: shared with the agent so that
        // interrupt mode can stop a running agent loop mid-iteration.
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(agent_id, session_llm, tools, self.memory.clone())
            .with_config(self.agent_config.clone())
            .with_reporter(Arc::new(octos_agent::SilentReporter))
            .with_shutdown(cancelled.clone())
            .with_system_prompt(system_prompt)
            // M8 fix-first item 8 (gap 1): wire the seeded per-actor
            // FileStateCache so file tools see resumed-state claims.
            .with_file_state_cache(file_state_cache.clone())
            // M8 fix-first item 8 (gap 2): wire the M8.7 disk router and
            // periodic summary generator so spawn_only background tasks
            // surface output and status to dashboards.
            .with_subagent_output_router(self.subagent_output_router.clone())
            .with_subagent_summary_generator(subagent_summary_generator);

        if let Some(ref embedder) = self.embedder {
            agent = agent.with_embedder(embedder.clone());
        }
        if let Some(ref hooks) = self.hooks {
            agent = agent.with_hooks(hooks.clone());
        }
        if let Some(ref ctx) = session_hook_context {
            agent = agent.with_hook_context(ctx.clone());
        }

        // Harness M6.3/M6.4: wire the declarative compaction runner when the
        // active workspace policy declares a compaction block. Selects the
        // LLM-iterative summarizer when the policy asks for it (hands in the
        // agent's LlmProvider); falls back to extractive otherwise.
        if let Some(ref workspace_policy) = active_workspace_policy {
            if let Some(compaction_policy) = workspace_policy.compaction.clone() {
                let runner = match compaction_policy.summarizer {
                    CompactionSummarizerKind::LlmIterative => {
                        CompactionRunner::with_provider(compaction_policy, agent.llm_provider())
                    }
                    CompactionSummarizerKind::Extractive => {
                        CompactionRunner::new(compaction_policy)
                    }
                }
                .with_workspace_policy(workspace_policy);
                agent = agent
                    .with_compaction_runner(Arc::new(runner))
                    .with_compaction_workspace(workspace_policy.clone());
            }
        }

        // Review A F-015: attach a cross-turn persistent retry state handle
        // so LoopRetryState buckets accumulate across consecutive
        // `process_message` / `run_task` calls for this session. The sidecar
        // is JSON so operators can inspect or purge it without opening redb;
        // the handle is read-through / write-back owned by the agent loop.
        let retry_state_path = retry_state_sidecar_path(&self.data_dir, &session_key);
        let retry_state_initial = load_retry_state(&retry_state_path);
        let persistent_retry_state = Arc::new(StdMutex::new(retry_state_initial));
        agent = agent.with_persistent_retry_state(persistent_retry_state.clone());

        // Wire the activate_tools back-reference now that tools are in Arc
        agent.wire_activate_tools();

        // Load per-user status configuration
        let user_status_config = UserStatusConfig::load(&self.data_dir, session_key.base_key());

        let actor = SessionActor {
            session_key: session_key.clone(),
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            inbox: rx,
            agent: Arc::new(agent),
            hooks: self.hooks.clone(),
            hook_context: session_hook_context,
            session_handle,
            llm_for_compaction: self.llm_for_compaction.clone(),
            out_tx: proxy_tx, // actor sends through proxy, not directly
            status_indicator,
            sender_user_id: sender_user_id.clone(),
            user_status_config,
            data_dir: self.data_dir.clone(),
            max_history: self.max_history.clone(),
            idle_timeout: self.idle_timeout,
            session_timeout: self.session_timeout,
            semaphore,
            global_shutdown: self.shutdown.clone(),
            cancelled,
            queue_mode: self.queue_mode,
            responsiveness: ResponsivenessObserver::new(),
            adaptive_router: self.adaptive_router.clone(),
            memory_store: self.memory_store.clone(),
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: self.active_sessions.clone(),
            user_workspace: user_workspace.clone(),
            cron_tool: cron_tool_ref,
            persistent_retry_state,
            retry_state_path: Some(retry_state_path),
            recovered_tasks: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        };

        // Spawn the outbound forwarding task — buffers messages from inactive sessions
        let fwd_session_key = session_key.clone();
        let fwd_out_tx = self.out_tx.clone();
        let fwd_active = self.active_sessions.clone();
        let fwd_pending = self.pending_messages.clone();
        let fwd_channel = channel.to_string();
        let fwd_chat_id = chat_id.to_string();
        tokio::spawn(outbound_forwarder(ForwarderParams {
            proxy_rx,
            out_tx: fwd_out_tx,
            session_key: fwd_session_key,
            channel: fwd_channel,
            chat_id: fwd_chat_id,
            active_sessions: fwd_active,
            pending_messages: fwd_pending,
            sender_user_id,
        }));

        let join_handle = tokio::spawn(actor.run());

        info!(session = %session_key, channel, chat_id, "spawned session actor");
        (tx, join_handle)
    }
}

/// Forwarding task: reads from the session's proxy channel and either delivers
/// messages directly (if this session is active) or buffers them.
async fn outbound_forwarder(params: ForwarderParams) {
    let ForwarderParams {
        mut proxy_rx,
        out_tx,
        session_key,
        channel,
        chat_id,
        active_sessions,
        pending_messages,
        sender_user_id,
    } = params;
    let my_topic = session_key.topic().unwrap_or("").to_string();
    let base_key = session_key.base_key().to_string();
    let key_str = session_key.to_string();

    while let Some(mut msg) = proxy_rx.recv().await {
        // Inject sender_user_id into outbound metadata so the channel
        // sends as the correct virtual user (appservice identity assertion).
        if let Some(ref uid) = sender_user_id {
            if let Some(obj) = msg.metadata.as_object_mut() {
                obj.insert(
                    METADATA_SENDER_USER_ID.to_string(),
                    serde_json::Value::String(uid.clone()),
                );
            }
        }
        let active_topic = active_sessions
            .read()
            .await
            .get_active_topic(&base_key)
            .to_string();

        if my_topic == active_topic {
            // Session is active — deliver immediately
            let _ = out_tx.send(msg).await;
        } else {
            // Session is inactive — buffer the message
            let mut pending = pending_messages.lock().await;
            let buf = pending.entry(key_str.clone()).or_default();
            let is_first = buf.is_empty();
            if buf.len() < MAX_PENDING_PER_SESSION {
                buf.push(msg);
            } else {
                warn!(session = %session_key, "pending buffer full, dropping message");
                // Replace the last buffered message with a truncation notice so the
                // user sees feedback when they switch to this session.
                if let Some(last) = buf.last_mut() {
                    last.content = format!(
                        "{}\n\n⚠️ Buffer full ({MAX_PENDING_PER_SESSION} messages). \
                         Some responses were dropped. Switch to this session to continue.",
                        last.content,
                    );
                }
            }
            drop(pending); // release lock before sending notification

            if is_first {
                let topic_label = if my_topic.is_empty() {
                    "(default)"
                } else {
                    &my_topic
                };
                let _ = out_tx
                    .send(OutboundMessage {
                        channel: channel.clone(),
                        chat_id: chat_id.clone(),
                        content: format!("📌 {topic_label} finished. /s {topic_label} to view."),
                        reply_to: None,
                        media: vec![],
                        metadata: system_notice_metadata(sender_user_id.as_deref()),
                    })
                    .await;
            }
        }
    }
}

// ── SessionActor ────────────────────────────────────────────────────────────

/// Long-lived task that processes all messages for one session.
struct SessionActor {
    session_key: SessionKey,
    channel: String,
    chat_id: String,

    inbox: mpsc::Receiver<ActorMessage>,

    agent: Arc<Agent>,
    hooks: Option<Arc<HookExecutor>>,
    hook_context: Option<HookContext>,

    /// Per-actor session handle — owns this session's data, no shared mutex.
    session_handle: Arc<Mutex<SessionHandle>>,
    llm_for_compaction: Arc<dyn LlmProvider>,

    out_tx: mpsc::Sender<OutboundMessage>,

    status_indicator: Option<Arc<StatusComposer>>,
    sender_user_id: Option<String>,
    /// Per-user status configuration (greeting, visibility toggles, custom layers).
    user_status_config: UserStatusConfig,
    /// Data directory for persisting user configs.
    data_dir: std::path::PathBuf,
    max_history: Arc<std::sync::atomic::AtomicUsize>,

    idle_timeout: Duration,
    session_timeout: Duration,
    semaphore: Arc<Semaphore>,
    /// Global shutdown flag (Ctrl+C, etc.)
    global_shutdown: Arc<AtomicBool>,
    /// Per-actor cancellation flag (only affects this session)
    cancelled: Arc<AtomicBool>,
    /// Queue mode for handling messages that arrive during active processing.
    queue_mode: QueueMode,
    /// Tracks LLM response latencies and detects sustained degradation.
    responsiveness: ResponsivenessObserver,
    /// Side-channel to AdaptiveRouter for toggling auto-protection.
    adaptive_router: Option<Arc<AdaptiveRouter>>,
    /// Memory store for saving long research reports out-of-band.
    memory_store: Option<Arc<MemoryStore>>,
    /// Active overflow task counter for concurrency limiting.
    active_overflow_tasks: Arc<std::sync::atomic::AtomicU32>,
    /// Cancellation flag for in-flight overflow tasks.
    /// Set when a slash command is handled so overflow responses don't
    /// interleave with command replies (GitHub issue #21).
    overflow_cancelled: Arc<AtomicBool>,
    /// Active session store — used to check if this session is currently active.
    /// When inactive, streaming edits are skipped so replies go through the
    /// proxy → pending buffer path and can be flushed on session switch.
    active_sessions: Arc<RwLock<ActiveSessionStore>>,
    /// Per-user workspace directory — the agent's sandboxed working directory.
    /// Media files uploaded by the user are copied here so read_file can access them.
    user_workspace: std::path::PathBuf,
    /// Per-session cron tool reference — updated with channel/chat_id on each message.
    cron_tool: Option<Arc<CronTool>>,
    /// Review A F-015: cross-turn persistent retry-bucket handle. The
    /// agent loop's `PersistentRetryStateGuard` hydrates from this at turn
    /// start and writes back on drop. We hold a clone of the same `Arc` so
    /// we can flush the state to a JSON sidecar after every turn.
    persistent_retry_state: Arc<StdMutex<LoopRetryState>>,
    /// Path of the retry-state JSON sidecar on disk. `None` when the path
    /// could not be resolved (e.g. unusual test data dirs); in that case
    /// the in-memory state still accumulates within this actor's lifetime
    /// but is not durable across process restarts.
    retry_state_path: Option<std::path::PathBuf>,
    /// Set of `task_id`s that have already triggered an automatic recovery
    /// turn (M8.9). Caps recovery at one attempt per task so a recovery
    /// turn that itself fails cannot ignite a runaway loop.
    recovered_tasks: Arc<StdMutex<std::collections::HashSet<String>>>,
}

impl SessionActor {
    async fn emit_hook_payload(&self, payload: HookPayload) {
        let Some(hooks) = self.hooks.as_ref() else {
            return;
        };
        let event = payload.event;
        match hooks.run(event, &payload).await {
            HookResult::Allow => {}
            HookResult::Modified(_) => {
                warn!(
                    session = %self.session_key,
                    event = ?event,
                    "lifecycle hook attempted to modify payload; ignoring"
                );
            }
            HookResult::Deny(reason) => {
                warn!(
                    session = %self.session_key,
                    event = ?event,
                    reason,
                    "lifecycle hook attempted to deny a non-blocking event"
                );
            }
            HookResult::Error(error) => {
                warn!(
                    session = %self.session_key,
                    event = ?event,
                    error,
                    "lifecycle hook failed"
                );
            }
        }
    }

    async fn emit_resume_hook(&self) {
        self.emit_hook_payload(HookPayload::on_resume(self.hook_context.as_ref()))
            .await;
    }

    async fn emit_turn_end_hook(&self, turn_summary: &str) {
        self.emit_hook_payload(HookPayload::on_turn_end(
            git_turn_summary(turn_summary),
            self.hook_context.as_ref(),
        ))
        .await;
    }

    async fn snapshot_workspace_turn_if_needed(
        &self,
        turn_summary: &str,
        reply_to: Option<String>,
    ) {
        if let Some(notice) = snapshot_workspace_turn_for_path(
            &self.session_key,
            self.user_workspace.clone(),
            turn_summary,
        )
        .await
        {
            emit_workspace_snapshot_notice(
                &self.out_tx,
                &self.channel,
                &self.chat_id,
                reply_to,
                self.sender_user_id.as_deref(),
                notice,
            )
            .await;
        }
    }

    /// Check if this session is currently the active session for its chat.
    /// When inactive, streaming edits bypass the pending buffer, so we must
    /// skip streaming and let the reply go through the proxy path.
    async fn is_active(&self) -> bool {
        let my_topic = self.session_key.topic().unwrap_or("");
        let base_key = self.session_key.base_key();
        let active_topic = self
            .active_sessions
            .read()
            .await
            .get_active_topic(base_key)
            .to_string();
        my_topic == active_topic
    }

    /// Reserve a recovery slot for a task. Returns `true` if this is the
    /// first recovery for the given task ID and the caller should proceed,
    /// `false` if a recovery has already been triggered (and the second
    /// signal should be dropped). Cap is one recovery attempt per task —
    /// see M8.9.
    fn claim_recovery_slot(&self, task_id: &str) -> bool {
        let mut guard = self
            .recovered_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(task_id.to_string())
    }

    /// Build a synthetic `InboundMessage` carrying the recovery prompt so
    /// the existing inbound pipeline (history persistence, agent loop)
    /// runs unchanged.
    ///
    /// Issue #738 fix: when `originating_client_message_id` is supplied,
    /// stamp it into `metadata.client_message_id` so `process_inbound`'s
    /// `inbound_client_message_id` helper reads it back and the recovery
    /// turn inherits the originating user turn's thread_id instead of
    /// `process_inbound` minting a fresh server UUIDv7. The eventual
    /// successful retry's deliverables (e.g. `_report.md`) then land
    /// under the original SPA bubble rather than an orphan thread_id.
    fn synthetic_recovery_inbound(
        &self,
        prompt: String,
        originating_client_message_id: Option<String>,
    ) -> InboundMessage {
        let mut metadata = serde_json::Map::new();
        metadata.insert("_recovery_turn".to_string(), serde_json::json!(true));
        if let Some(cmid) = originating_client_message_id {
            if !cmid.is_empty() {
                metadata.insert(
                    "client_message_id".to_string(),
                    serde_json::Value::String(cmid),
                );
            }
        }
        InboundMessage {
            channel: self.channel.clone(),
            sender_id: "octos-runtime".to_string(),
            chat_id: self.chat_id.clone(),
            content: prompt,
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::Value::Object(metadata),
            message_id: None,
        }
    }

    async fn run(mut self) {
        self.emit_resume_hook().await;
        loop {
            tokio::select! {
                msg = self.inbox.recv() => {
                    match msg {
                        Some(ActorMessage::Inbound {
                            message,
                            image_media,
                            attachment_media,
                            attachment_prompt,
                        }) => {
                            // Update cron tool context with current channel/chat_id
                            // so new cron jobs inherit the correct delivery target.
                            if let Some(ref cron) = self.cron_tool {
                                if !self.channel.is_empty() && !self.chat_id.is_empty() {
                                    cron.set_context(&self.channel, &self.chat_id);
                                }
                            }

                            // Check for abort trigger before processing
                            if octos_core::is_abort_trigger(&message.content) {
                                debug!(session = %self.session_key, "abort trigger detected");
                                self.cancelled.store(true, Ordering::Release);
                                let _ = self.out_tx.send(OutboundMessage {
                                    channel: self.channel.clone(),
                                    chat_id: self.chat_id.clone(),
                                    content: octos_core::abort_response(&message.content).to_string(),
                                    reply_to: None,
                                    media: vec![],
                                    metadata: serde_json::json!({}),
                                }).await;
                                // Reset for next message
                                self.cancelled.store(false, Ordering::Release);
                                continue;
                            }

                            // Handle slash commands (no LLM round-trip)
                            if self.try_handle_command(&message).await {
                                // Cancel any in-flight overflow tasks so their
                                // responses don't preempt the command reply (#21).
                                self.overflow_cancelled.store(true, Ordering::Release);
                                // Send completion signal so the web client's SSE stream closes
                                if self.channel == "api" {
                                    let _ = self.out_tx.send(OutboundMessage {
                                        channel: self.channel.clone(),
                                        chat_id: self.chat_id.clone(),
                                        content: String::new(),
                                        reply_to: None,
                                        media: vec![],
                                        metadata: serde_json::json!({"_completion": true}),
                                    }).await;
                                }
                                continue;
                            }

                            // Drain any queued messages according to queue mode
                            let (
                                final_message,
                                final_media,
                                final_attachment_media,
                                final_attachment_prompt,
                            ) = self
                                .drain_queue(
                                    message,
                                    image_media,
                                    attachment_media,
                                    attachment_prompt,
                                )
                                .await;

                            // Copy non-image attachments into the agent workspace so
                            // tools can resolve them by filename without path hints.
                            let final_attachment_media =
                                self.copy_media_to_workspace(final_attachment_media);

                            // Most API sessions use speculative overflow so the web
                            // client stays responsive during long tool calls. Contract-
                            // owned slides/site sessions are the exception: allowing
                            // overflow there can replay artifact-producing turns and
                            // duplicate final deliveries, so keep those serialized.
                            if !topic_requires_serial_delivery(self.session_key.topic())
                                && (self.queue_mode == QueueMode::Speculative
                                    || self.channel == "api")
                            {
                                self.process_inbound_speculative(
                                    final_message,
                                    final_media,
                                    final_attachment_media,
                                    final_attachment_prompt,
                                )
                                .await;
                            } else {
                                self.process_inbound(
                                    final_message,
                                    final_media,
                                    final_attachment_media,
                                    final_attachment_prompt,
                                )
                                .await;
                            }
                        }
                        Some(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                            kind,
                            media,
                            originating_thread_id,
                            ack,
                        }) => {
                            let persisted = self
                                .handle_background_result(
                                    &task_label,
                                    &content,
                                    kind,
                                    media,
                                    originating_thread_id,
                                )
                                .await;
                            if let Some(ack) = ack {
                                let _ = ack.send(persisted);
                            }
                        }
                        Some(ActorMessage::TaskStatusChanged { task_json }) => {
                            // Push task status change to the web client via SSE
                            let _ = self.out_tx.send(octos_core::OutboundMessage {
                                channel: self.channel.clone(),
                                chat_id: self.chat_id.clone(),
                                content: String::new(),
                                reply_to: None,
                                media: vec![],
                                metadata: serde_json::json!({
                                    "topic": self.session_key.topic(),
                                    "_task_status": task_json
                                }),
                            }).await;
                        }
                        Some(ActorMessage::RecoveryHint {
                            task_id,
                            tool_name,
                            prompt,
                            originating_client_message_id,
                        }) => {
                            // Cap recovery at one attempt per task to avoid
                            // runaway loops if the recovery turn itself
                            // fails. Subsequent failures from the same task
                            // ID are silently dropped here.
                            if !self.claim_recovery_slot(&task_id) {
                                debug!(
                                    session = %self.session_key,
                                    task_id,
                                    tool_name,
                                    "skipping duplicate recovery hint"
                                );
                                continue;
                            }
                            debug!(
                                session = %self.session_key,
                                task_id,
                                tool_name,
                                originating_client_message_id =
                                    originating_client_message_id.as_deref().unwrap_or("<none>"),
                                "enqueueing synthetic recovery turn"
                            );
                            let synthetic = self.synthetic_recovery_inbound(
                                prompt,
                                originating_client_message_id,
                            );
                            let final_attachment_media = self
                                .copy_media_to_workspace(Vec::new());
                            self.process_inbound(
                                synthetic,
                                Vec::new(),
                                final_attachment_media,
                                None,
                            )
                            .await;
                        }
                        Some(ActorMessage::Cancel) => {
                            debug!(session = %self.session_key, "cancel requested");
                            self.cancelled.store(true, Ordering::Release);
                        }
                        None => {
                            // All senders dropped
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(self.idle_timeout) => {
                    record_timeout("idle_actor");
                    debug!(session = %self.session_key, "idle timeout, shutting down actor");
                    break;
                }
            }

            if self.global_shutdown.load(Ordering::Acquire)
                || self.cancelled.load(Ordering::Acquire)
            {
                break;
            }
        }

        debug!(session = %self.session_key, "actor exiting");
    }

    /// Handle slash commands that don't need an LLM round-trip.
    /// Returns `true` if the message was consumed as a command.
    async fn try_handle_command(&mut self, message: &InboundMessage) -> bool {
        let text = message.content.trim();
        if !text.starts_with('/') {
            return false;
        }

        let parts: Vec<&str> = text.split_whitespace().collect();
        let cmd = parts[0];

        match cmd {
            "/adaptive" => {
                self.handle_adaptive_command(&parts[1..]).await;
                true
            }
            "/queue" => {
                self.handle_queue_command(&parts[1..]).await;
                true
            }
            "/status" => {
                self.handle_status_command(&parts[1..]).await;
                true
            }
            "/reset" => {
                self.handle_reset_command().await;
                true
            }
            "/thinking" => {
                self.handle_thinking_command(&parts[1..]).await;
                true
            }
            _ => {
                // Unknown slash command — show help instead of passing to LLM
                self.send_reply(
                    "Unknown command. Available commands:\n\
                     /new [name] — start a new session\n\
                     /s [name] — switch to a session\n\
                     /sessions — list all sessions\n\
                     /back — return to default session\n\
                     /delete — delete current session\n\
                     /soul [text] — view or set persona\n\
                     /status — show agent status\n\
                     /adaptive — view adaptive routing\n\
                     /reset — reset session state\n\
                     /help — show this help",
                )
                .await;
                true
            }
        }
    }

    /// `/adaptive` — view or toggle adaptive routing features.
    ///
    /// Usage:
    ///   /adaptive                       — show current status
    ///   /adaptive circuit on|off        — toggle auto circuit breaker
    ///   /adaptive lane on|off           — toggle lane changing
    ///   /adaptive qos on|off            — toggle QoS ranking
    async fn handle_adaptive_command(&self, args: &[&str]) {
        let Some(ref router) = self.adaptive_router else {
            self.send_reply("Adaptive routing is not enabled.").await;
            return;
        };

        if args.is_empty() {
            // Show status
            let status = router.adaptive_status();
            let provider = router.current_provider_name();
            let snapshots = router.metrics_snapshots();

            let mut lines = vec![
                "**Adaptive Routing**".to_string(),
                format!("  mode:        {}", status.mode),
                format!(
                    "  qos ranking: {}",
                    if status.qos_ranking { "on" } else { "off" }
                ),
                format!("  current:     {provider}"),
            ];

            if !snapshots.is_empty() {
                lines.push(String::new());
                lines.push("**Providers**".to_string());
                for (name, model, snap) in &snapshots {
                    lines.push(format!(
                        "  {name} ({model}): latency={:.0}ms ok={} err={} {}",
                        snap.latency_ema_ms,
                        snap.success_count,
                        snap.failure_count,
                        if snap.consecutive_failures >= status.failure_threshold {
                            "⛔ OPEN"
                        } else {
                            "✅"
                        },
                    ));
                }
            }

            self.send_reply(&lines.join("\n")).await;
            return;
        }

        match args[0] {
            // Mode switching: /adaptive off|hedge|lane
            "off" => {
                router.set_mode(AdaptiveMode::Off);
                self.send_reply("Adaptive mode: off (static priority, failover only)")
                    .await;
            }
            "hedge" | "race" | "circuit" => {
                router.set_mode(AdaptiveMode::Hedge);
                let status = router.adaptive_status();
                if status.provider_count < 2 {
                    self.send_reply("Adaptive mode: hedge (race 2 providers, take winner)\n⚠️ Only 1 provider configured — hedge needs ≥2 to race. Currently behaves like off mode.").await;
                } else {
                    self.send_reply(&format!(
                        "Adaptive mode: hedge (race 2 of {} providers, take winner)",
                        status.provider_count
                    ))
                    .await;
                }
            }
            "lane" => {
                router.set_mode(AdaptiveMode::Lane);
                let status = router.adaptive_status();
                if status.provider_count < 2 {
                    self.send_reply("Adaptive mode: lane (score-based provider selection)\n⚠️ Only 1 provider configured — lane needs ≥2 to compare. Currently behaves like off mode.").await;
                } else {
                    self.send_reply(&format!(
                        "Adaptive mode: lane (score-based selection across {} providers)",
                        status.provider_count
                    ))
                    .await;
                }
            }
            // QoS toggle: /adaptive qos [on|off]
            "qos" => {
                if let Some(value) = args.get(1) {
                    let enabled = match *value {
                        "on" | "true" | "1" => true,
                        "off" | "false" | "0" => false,
                        other => {
                            self.send_reply(&format!("Invalid value: {other}. Use: on/off"))
                                .await;
                            return;
                        }
                    };
                    router.set_qos_ranking(enabled);
                    self.send_reply(&format!(
                        "QoS ranking: {}",
                        if enabled { "on" } else { "off" }
                    ))
                    .await;
                } else {
                    let on = router.adaptive_status().qos_ranking;
                    self.send_reply(&format!("QoS ranking: {}", if on { "on" } else { "off" }))
                        .await;
                }
            }
            other => {
                self.send_reply(&format!(
                    "Unknown option: {other}\nUsage: /adaptive [off|hedge|lane|qos [on|off]]"
                ))
                .await;
            }
        }
    }

    /// `/queue` — view or change the queue mode.
    ///
    /// Usage:
    ///   /queue                          — show current mode
    ///   /queue followup|collect|steer|interrupt
    async fn handle_queue_command(&mut self, args: &[&str]) {
        if args.is_empty() {
            self.send_reply(&format!("Queue mode: {:?}", self.queue_mode))
                .await;
            return;
        }

        let mode = match args[0] {
            "followup" => QueueMode::Followup,
            "collect" => QueueMode::Collect,
            "steer" => QueueMode::Steer,
            "interrupt" => QueueMode::Interrupt,
            "spec" | "speculative" => QueueMode::Speculative,
            other => {
                self.send_reply(&format!(
                    "Unknown mode: {other}. Use: followup, collect, steer, interrupt, spec"
                ))
                .await;
                return;
            }
        };

        self.queue_mode = mode;
        self.send_reply(&format!("Queue mode set to: {:?}", mode))
            .await;
    }

    /// `/status` — view or configure per-user status layers.
    ///
    /// Usage:
    ///   /status                        — show current config
    ///   /status greeting <text>        — set greeting template
    ///   /status provider on|off        — toggle provider layer
    ///   /status metrics on|off         — toggle metrics layer
    ///   /status words <w1,w2,...>       — set custom status words
    ///   /status add <id> <priority> <text> — add custom layer
    ///   /status remove <id>            — remove custom layer
    ///   /status reset                  — reset to defaults
    async fn handle_status_command(&mut self, args: &[&str]) {
        use crate::status_layers::{CustomLayerDef, LayerPolicy};

        if args.is_empty() {
            let cfg = &self.user_status_config;
            let mut lines = vec![
                "**Status Config**".to_string(),
                format!(
                    "Greeting: {}",
                    cfg.greeting_template.as_deref().unwrap_or("(none)")
                ),
                format!("Provider visible: {}", cfg.provider_visible),
                format!("Metrics visible: {}", cfg.metrics_visible),
                format!("Greeting duration: {}s", cfg.greeting_duration_secs),
            ];
            if let Some(ref words) = cfg.status_words {
                lines.push(format!("Words: {}", words.join(", ")));
            }
            if let Some(ref locale) = cfg.locale {
                lines.push(format!("Locale: {locale}"));
            }
            for custom in &cfg.custom_layers {
                lines.push(format!(
                    "Custom layer `{}` (p={}): {}",
                    custom.id, custom.priority, custom.content
                ));
            }
            self.send_reply(&lines.join("\n")).await;
            return;
        }

        match args[0] {
            "greeting" => {
                if args.len() < 2 {
                    self.send_reply("Usage: /status greeting <text>  (or /status greeting off)")
                        .await;
                    return;
                }
                let text = args[1..].join(" ");
                if text == "off" || text == "none" {
                    self.user_status_config.greeting_template = None;
                    self.send_reply("Greeting disabled.").await;
                } else {
                    self.user_status_config.greeting_template = Some(text.clone());
                    self.send_reply(&format!("Greeting set: {text}")).await;
                }
            }
            "provider" => {
                let on = match args.get(1).copied() {
                    Some("on" | "true" | "1") => true,
                    Some("off" | "false" | "0") => false,
                    _ => {
                        self.send_reply("Usage: /status provider on|off").await;
                        return;
                    }
                };
                self.user_status_config.provider_visible = on;
                self.send_reply(&format!(
                    "Provider layer: {}",
                    if on { "visible" } else { "hidden" }
                ))
                .await;
            }
            "metrics" => {
                let on = match args.get(1).copied() {
                    Some("on" | "true" | "1") => true,
                    Some("off" | "false" | "0") => false,
                    _ => {
                        self.send_reply("Usage: /status metrics on|off").await;
                        return;
                    }
                };
                self.user_status_config.metrics_visible = on;
                self.send_reply(&format!(
                    "Metrics layer: {}",
                    if on { "visible" } else { "hidden" }
                ))
                .await;
            }
            "words" => {
                if args.len() < 2 {
                    self.send_reply("Usage: /status words word1,word2,...")
                        .await;
                    return;
                }
                let words: Vec<String> = args[1..]
                    .join(" ")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if words.is_empty() {
                    self.user_status_config.status_words = None;
                    self.send_reply("Status words reset to default.").await;
                } else {
                    let preview = words.join(", ");
                    self.user_status_config.status_words = Some(words);
                    self.send_reply(&format!("Status words: {preview}")).await;
                }
            }
            "add" => {
                // /status add <id> <priority> <text>
                if args.len() < 4 {
                    self.send_reply("Usage: /status add <id> <priority> <text>")
                        .await;
                    return;
                }
                let id = args[1].to_string();
                let priority: u8 = match args[2].parse() {
                    Ok(p) => p,
                    Err(_) => {
                        self.send_reply("Priority must be a number 0-255.").await;
                        return;
                    }
                };
                let content = args[3..].join(" ");
                // Remove existing layer with same ID
                self.user_status_config.custom_layers.retain(|l| l.id != id);
                self.user_status_config.custom_layers.push(CustomLayerDef {
                    id: id.clone(),
                    priority,
                    policy: LayerPolicy::Fixed,
                    content: content.clone(),
                });
                self.send_reply(&format!("Added layer `{id}` (p={priority}): {content}"))
                    .await;
            }
            "remove" => {
                if args.len() < 2 {
                    self.send_reply("Usage: /status remove <id>").await;
                    return;
                }
                let id = args[1];
                let before = self.user_status_config.custom_layers.len();
                self.user_status_config.custom_layers.retain(|l| l.id != id);
                if self.user_status_config.custom_layers.len() < before {
                    self.send_reply(&format!("Removed layer `{id}`.")).await;
                } else {
                    self.send_reply(&format!("No custom layer `{id}` found."))
                        .await;
                }
            }
            "duration" => {
                if let Some(secs) = args.get(1).and_then(|s| s.parse::<u64>().ok()) {
                    self.user_status_config.greeting_duration_secs = secs;
                    self.send_reply(&format!("Greeting duration: {secs}s"))
                        .await;
                } else {
                    self.send_reply("Usage: /status duration <seconds>").await;
                    return;
                }
            }
            "locale" => {
                if let Some(loc) = args.get(1) {
                    if *loc == "auto" || *loc == "off" {
                        self.user_status_config.locale = None;
                        self.send_reply("Locale: auto-detect").await;
                    } else {
                        self.user_status_config.locale = Some(loc.to_string());
                        self.send_reply(&format!("Locale: {loc}")).await;
                    }
                } else {
                    self.send_reply("Usage: /status locale <en|zh|auto>").await;
                    return;
                }
            }
            "reset" => {
                self.user_status_config = UserStatusConfig::default();
                self.send_reply("Status config reset to defaults.").await;
            }
            other => {
                self.send_reply(&format!(
                    "Unknown status subcommand: {other}\n\
                    Usage: /status [greeting|provider|metrics|words|add|remove|duration|locale|reset]"
                )).await;
                return;
            }
        }

        // Persist changes
        let base_key = self.session_key.base_key();
        if let Err(e) = self.user_status_config.save(&self.data_dir, base_key) {
            warn!(error = %e, "failed to save user status config");
        }
    }

    /// `/reset` — reset session state for test isolation.
    ///
    /// Resets queue mode to default (collect) and clears conversation
    /// history for the current session. Does NOT touch the adaptive
    /// router — that's a gateway-level shared resource.
    async fn handle_reset_command(&mut self) {
        // Reset queue mode to default
        self.queue_mode = QueueMode::default();

        // Clear conversation history
        {
            let mut handle = self.session_handle.lock().await;
            if let Err(e) = handle.clear().await {
                warn!(error = %e, "failed to clear session history");
            }
        }

        self.send_reply("Reset: queue=collect, adaptive=off, history cleared.")
            .await;
    }

    /// `/thinking` — toggle display of model reasoning/thinking content.
    ///
    /// Usage:
    ///   /thinking          — show current state
    ///   /thinking on       — show thinking content in responses
    ///   /thinking off      — hide thinking content (default)
    async fn handle_thinking_command(&mut self, args: &[&str]) {
        match args.first().copied() {
            Some("on" | "true" | "1") => {
                self.user_status_config.show_thinking = true;
                self.send_reply("💭 Thinking display: **on** — reasoning content will be shown.")
                    .await;
            }
            Some("off" | "false" | "0") => {
                self.user_status_config.show_thinking = false;
                self.send_reply("💭 Thinking display: **off** — reasoning content will be hidden.")
                    .await;
            }
            None => {
                let state = if self.user_status_config.show_thinking {
                    "on"
                } else {
                    "off"
                };
                self.send_reply(&format!(
                    "💭 Thinking display: **{state}**\n\nUsage: `/thinking on` or `/thinking off`"
                ))
                .await;
            }
            _ => {
                self.send_reply("Usage: `/thinking on|off`").await;
            }
        }
        let base_key = self.session_key.base_key();
        if let Err(e) = self.user_status_config.save(&self.data_dir, base_key) {
            warn!(error = %e, "failed to save user status config");
        }
    }

    /// Send a short reply to the user (for command responses).
    async fn send_reply(&self, content: &str) {
        let _ = self
            .out_tx
            .send(OutboundMessage {
                channel: self.channel.clone(),
                chat_id: self.chat_id.clone(),
                content: content.to_string(),
                reply_to: None,
                media: vec![],
                metadata: serde_json::json!({}),
            })
            .await;

        // Send completion marker so the API channel closes the SSE stream.
        if self.channel == "api" {
            let _ = self
                .out_tx
                .send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: String::new(),
                    reply_to: None,
                    media: vec![],
                    metadata: serde_json::json!({"_completion": true}),
                })
                .await;
        }
    }

    /// Drain any already-queued messages from the inbox and combine them
    /// with the current message according to the configured queue mode.
    ///
    /// - Followup: return the message as-is (queued messages processed next iteration)
    /// - Collect: batch all queued messages into one combined prompt
    /// - Steer: discard current message, use the newest queued message instead
    /// - Interrupt: same as Steer (cancellation already handled at dispatch level)
    async fn drain_queue(
        &mut self,
        message: InboundMessage,
        image_media: Vec<String>,
        attachment_media: Vec<String>,
        attachment_prompt: Option<String>,
    ) -> (InboundMessage, Vec<String>, Vec<String>, Option<String>) {
        match self.queue_mode {
            QueueMode::Followup | QueueMode::Speculative => {
                (message, image_media, attachment_media, attachment_prompt)
            }
            QueueMode::Collect => {
                let mut combined_content = message.content.clone();
                let mut combined_media = image_media;
                let mut combined_attachment_media = attachment_media;
                let mut combined_attachment_prompt = attachment_prompt;
                let mut count = 0u32;

                // Non-blocking drain of queued inbound messages
                loop {
                    match self.inbox.try_recv() {
                        Ok(ActorMessage::Inbound {
                            message: queued,
                            image_media: queued_media,
                            attachment_media: queued_attachment_media,
                            attachment_prompt: queued_attachment_prompt,
                        }) => {
                            if octos_core::is_abort_trigger(&queued.content) {
                                debug!(session = %self.session_key, "abort in queue, cancelling batch");
                                self.cancelled.store(true, Ordering::Release);
                                break;
                            }
                            count += 1;
                            combined_content
                                .push_str(&format!("\n---\nQueued #{count}: {}", queued.content));
                            combined_media.extend(queued_media);
                            combined_attachment_media.extend(queued_attachment_media);
                            combined_attachment_prompt = merge_attachment_prompt_summaries(
                                combined_attachment_prompt,
                                queued_attachment_prompt,
                            );
                        }
                        Ok(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                            kind,
                            media,
                            originating_thread_id,
                            ack,
                        }) => {
                            let persisted = self
                                .handle_background_result(
                                    &task_label,
                                    &content,
                                    kind,
                                    media,
                                    originating_thread_id,
                                )
                                .await;
                            if let Some(ack) = ack {
                                let _ = ack.send(persisted);
                            }
                        }
                        Ok(ActorMessage::TaskStatusChanged { .. }) => {
                            // Ignore in drain — status is pushed via the main loop
                        }
                        Ok(ActorMessage::RecoveryHint {
                            task_id, tool_name, ..
                        }) => {
                            // Drain context: a turn is already running. The
                            // claim guarantees we won't try to recover this
                            // task again later. Trace and drop — the LLM
                            // will see the failure via TaskStatusChanged.
                            debug!(
                                session = %self.session_key,
                                task_id,
                                tool_name,
                                "dropping recovery hint received during drain"
                            );
                            self.claim_recovery_slot(&task_id);
                        }
                        Ok(ActorMessage::Cancel) => {
                            self.cancelled.store(true, Ordering::Release);
                            break;
                        }
                        Err(_) => break, // inbox empty
                    }
                }
                let mut msg = message;
                msg.content = combined_content;
                (
                    msg,
                    combined_media,
                    combined_attachment_media,
                    combined_attachment_prompt,
                )
            }
            QueueMode::Steer | QueueMode::Interrupt => {
                // Coalescing delay: give rapid follow-up messages time to arrive
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                let mut latest_message = message;
                let mut latest_media = image_media;
                let mut latest_attachment_media = attachment_media;
                let mut latest_attachment_prompt = attachment_prompt;

                // Non-blocking drain: keep only the newest inbound message
                loop {
                    match self.inbox.try_recv() {
                        Ok(ActorMessage::Inbound {
                            message: queued,
                            image_media: queued_media,
                            attachment_media: queued_attachment_media,
                            attachment_prompt: queued_attachment_prompt,
                        }) => {
                            if octos_core::is_abort_trigger(&queued.content) {
                                debug!(session = %self.session_key, "abort in queue, cancelling");
                                self.cancelled.store(true, Ordering::Release);
                                break;
                            }
                            debug!(session = %self.session_key, "steer: replacing with newer message");
                            latest_message = queued;
                            latest_media = queued_media;
                            latest_attachment_media = queued_attachment_media;
                            latest_attachment_prompt = queued_attachment_prompt;
                        }
                        Ok(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                            kind,
                            media,
                            originating_thread_id,
                            ack,
                        }) => {
                            let persisted = self
                                .handle_background_result(
                                    &task_label,
                                    &content,
                                    kind,
                                    media,
                                    originating_thread_id,
                                )
                                .await;
                            if let Some(ack) = ack {
                                let _ = ack.send(persisted);
                            }
                        }
                        Ok(ActorMessage::TaskStatusChanged { .. }) => {
                            // Ignore in drain — status is pushed via the main loop
                        }
                        Ok(ActorMessage::RecoveryHint {
                            task_id, tool_name, ..
                        }) => {
                            // Drain context: a turn is already running. The
                            // claim guarantees we won't try to recover this
                            // task again later. Trace and drop — the LLM
                            // will see the failure via TaskStatusChanged.
                            debug!(
                                session = %self.session_key,
                                task_id,
                                tool_name,
                                "dropping recovery hint received during drain"
                            );
                            self.claim_recovery_slot(&task_id);
                        }
                        Ok(ActorMessage::Cancel) => {
                            self.cancelled.store(true, Ordering::Release);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                (
                    latest_message,
                    latest_media,
                    latest_attachment_media,
                    latest_attachment_prompt,
                )
            }
        }
    }

    /// Persist an assistant-visible background result and emit the matching
    /// committed session-result event metadata for the web/runtime surfaces.
    ///
    /// M8.10 follow-up (#649): `originating_thread_id` is the
    /// `client_message_id` of the user message that started the background
    /// task. When present it is stamped onto the OutboundMessage metadata
    /// so the api_channel routes the wire-side SSE event under the correct
    /// turn — even when subsequent unrelated user turns have rotated the
    /// per-chat sticky thread_id.
    async fn deliver_background_notification(
        &self,
        content: String,
        media: Vec<String>,
        originating_thread_id: Option<String>,
    ) -> bool {
        let content = finalize_assistant_content(&self.session_key, &self.user_workspace, &content);
        let persisted = persist_assistant_message(
            &self.session_handle,
            &self.session_key,
            &self.data_dir,
            content.clone(),
            media.clone(),
            originating_thread_id.clone(),
        )
        .await;

        let Some(persisted_message) = persisted else {
            record_result_delivery(
                "background_notification",
                "history_not_persisted",
                "notification",
            );
            warn!(
                session = %self.session_key,
                "skipping background notification fanout because history was not persisted"
            );
            return false;
        };

        let mut metadata = serde_json::json!({
            "topic": self.session_key.topic(),
            "_history_persisted": true,
            "_session_result": {
                "seq": persisted_message.seq,
                "role": "assistant",
                "content": content.clone(),
                "timestamp": persisted_message.timestamp.to_rfc3339(),
                "media": media.clone(),
            }
        });

        // M8.10 follow-up (#649): stamp `thread_id` onto the OutboundMessage
        // metadata so the api_channel resolves it via the explicit-metadata
        // path (NOT the per-chat sticky-map fallback). Without this stamp,
        // a deep_research / spawn_only result that completes after later
        // user turns inherits the WRONG turn's thread_id from the sticky
        // map (cf. live mini3 trace, 2026-04-29). The non-empty guard mirrors
        // `persist_assistant_message`'s — wire and disk agree on what counts
        // as a usable origin id, so a degenerate `Some("")` falls through to
        // the api_channel sticky-map fallback rather than poisoning routing.
        if let Some(tid) = originating_thread_id
            .as_deref()
            .filter(|tid| !tid.is_empty())
        {
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert(
                    "thread_id".to_string(),
                    serde_json::Value::String(tid.to_string()),
                );
                if let Some(sr) = obj
                    .get_mut("_session_result")
                    .and_then(|v| v.as_object_mut())
                {
                    sr.insert(
                        "thread_id".to_string(),
                        serde_json::Value::String(tid.to_string()),
                    );
                }
            }
        }

        let _ = send_outbound_with_timeout(
            &self.session_key,
            &self.out_tx,
            OutboundMessage {
                channel: self.channel.clone(),
                chat_id: self.chat_id.clone(),
                content,
                reply_to: None,
                media,
                metadata,
            },
            "background_notification",
        )
        .await;

        true
    }

    async fn handle_background_result(
        &self,
        task_label: &str,
        content: &str,
        kind: BackgroundResultKind,
        media: Vec<String>,
        originating_thread_id: Option<String>,
    ) -> bool {
        if kind == BackgroundResultKind::Notification {
            self.deliver_background_notification(content.to_string(), media, originating_thread_id)
                .await
        } else {
            let report_message = self
                .prepare_background_report_result(task_label, content)
                .await;
            self.deliver_background_notification(report_message, Vec::new(), originating_thread_id)
                .await
        }
    }

    async fn prepare_background_report_result(&self, task_label: &str, content: &str) -> String {
        const SUMMARY_THRESHOLD: usize = 1000;
        if content.len() > SUMMARY_THRESHOLD {
            // Save full report to memory bank
            let slug = task_label
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .to_lowercase();
            let slug = slug.trim_matches('-').to_string();

            if let Some(ref ms) = self.memory_store {
                let report_md = format!(
                    "# {task_label}\n\n_Generated: {}_\n\n{content}",
                    chrono::Utc::now().format("%Y-%m-%d %H:%M UTC"),
                );
                if let Err(e) = ms.write_entity(&slug, &report_md).await {
                    warn!(session = %self.session_key, error = %e, "failed to save report to memory bank");
                } else {
                    info!(session = %self.session_key, slug = %slug, len = content.len(), "saved report to memory bank");
                }
            }

            let preview: String = content.chars().take(300).collect();
            format!(
                "✅ **{task_label}** completed.\n\n{preview}...\n\n_Full report saved. Ask me to recall it for details._",
            )
        } else {
            format!("✅ **{task_label}** completed.\n\n{content}")
        }
    }

    /// Copy media files from their original location (e.g. profile media_dir)
    /// into the agent's sandboxed `user_workspace` so that `read_file` and
    /// other cwd-bound tools can access them.  Returns the updated paths.
    fn copy_media_to_workspace(&self, media: Vec<String>) -> Vec<String> {
        media
            .into_iter()
            .map(|path| {
                let resolved = octos_bus::file_handle::resolve_upload_reference(&path)
                    .map(|candidate| candidate.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                let src = std::path::Path::new(&resolved);
                if !src.exists() {
                    return resolved;
                }
                let Some(filename) = src.file_name() else {
                    return resolved;
                };
                let dest = self.user_workspace.join(filename);
                match std::fs::copy(src, &dest) {
                    Ok(_) => {
                        debug!(
                            session = %self.session_key,
                            src = %src.display(),
                            dest = %dest.display(),
                            "copied media file to workspace"
                        );
                        dest.to_string_lossy().into_owned()
                    }
                    Err(e) => {
                        warn!(
                            session = %self.session_key,
                            src = %src.display(),
                            error = %e,
                            "failed to copy media to workspace, using original path"
                        );
                        resolved
                    }
                }
            })
            .collect()
    }

    fn build_turn_attachment_context(
        &self,
        attachment_media: Vec<String>,
        attachment_prompt: Option<String>,
    ) -> TurnAttachmentContext {
        let mut audio_attachment_paths = Vec::new();
        let mut file_attachment_paths = Vec::new();
        for path in &attachment_media {
            if octos_bus::media::is_audio(path) {
                audio_attachment_paths.push(path.clone());
            } else {
                file_attachment_paths.push(path.clone());
            }
        }

        TurnAttachmentContext {
            attachment_paths: attachment_media,
            audio_attachment_paths,
            file_attachment_paths,
            prompt_summary: attachment_prompt,
        }
    }

    fn persisted_user_content(
        inbound: &InboundMessage,
        image_media: &[String],
        attachment_media: &[String],
    ) -> String {
        if inbound.content.is_empty() && !image_media.is_empty() {
            "[User sent an image]".to_string()
        } else if inbound.content.is_empty() && !attachment_media.is_empty() {
            "[User sent attachments]".to_string()
        } else {
            inbound.content.clone()
        }
    }

    fn forced_background_workflow_for_turn(
        &self,
        inbound: &InboundMessage,
        image_media: &[String],
        attachment_media: &[String],
    ) -> Option<WorkflowInstance> {
        if !image_media.is_empty() || !attachment_media.is_empty() {
            return None;
        }
        if self.channel == "system" {
            return None;
        }
        WorkflowKind::detect_forced_background(&inbound.content).map(WorkflowKind::build)
    }

    async fn maybe_start_forced_background_workflow(
        &self,
        inbound: &InboundMessage,
        image_media: &[String],
        attachment_media: &[String],
        attachment_prompt: Option<&str>,
        persisted_user_content: &str,
        reply_to: Option<String>,
    ) -> bool {
        let Some(workflow) =
            self.forced_background_workflow_for_turn(inbound, image_media, attachment_media)
        else {
            return false;
        };

        let mut task = inbound.content.clone();
        if let Some(prompt) = attachment_prompt.filter(|value| !value.trim().is_empty()) {
            task.push_str("\n\nAttachment context:\n");
            task.push_str(prompt);
        }

        let workflow_label = workflow.label.clone();
        let workflow_ack = workflow.ack_message.clone();
        let args = serde_json::json!({
            "task": task,
            "label": workflow_label,
            "mode": "background",
            "allowed_tools": workflow.allowed_tools.clone(),
            "additional_instructions": workflow.additional_instructions.clone(),
            "workflow": workflow.clone(),
        });

        let tool_registry = self.agent.tool_registry();
        let spawn_result = match tool_registry.execute("spawn", &args).await {
            Ok(result) if result.success => result,
            Ok(result) => {
                warn!(
                    session = %self.session_key,
                    workflow = %workflow.label,
                    error = %result.output,
                    "forced background spawn returned failure"
                );
                return false;
            }
            Err(error) => {
                warn!(
                    session = %self.session_key,
                    workflow = %workflow.label,
                    error = %error,
                    "forced background spawn failed"
                );
                return false;
            }
        };

        let client_message_id = inbound_client_message_id(inbound);
        let user_msg = Message {
            role: MessageRole::User,
            content: persisted_user_content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: client_message_id.clone(),
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        let user_msg_timestamp = user_msg.timestamp;
        let user_seq = {
            let mut handle = self.session_handle.lock().await;
            let session = handle.get_or_create();
            if session.summary.is_none() && !persisted_user_content.trim().is_empty() {
                session.summary = Some(persisted_user_content.chars().take(100).collect());
            }
            match handle.add_message_with_seq(user_msg).await {
                Ok(seq) => Some(seq),
                Err(error) => {
                    warn!(session = %self.session_key, error = %error, "failed to persist user message for forced background workflow");
                    None
                }
            }
        };

        // Restore the forced-background user-message session_result emission
        // dropped by 14ac3f3a. Same reasoning as the overflow path: the web
        // client needs a routing signal so the workflow's spawn_only progress
        // events bind to this user message's bubble, not a stale primary.
        // See #616.
        if let Some(seq) = user_seq {
            let mut session_result = serde_json::json!({
                "seq": seq,
                "role": "user",
                "content": persisted_user_content.to_string(),
                "timestamp": user_msg_timestamp.to_rfc3339(),
                "media": Vec::<String>::new(),
            });
            if let Some(cmid) = client_message_id.as_deref() {
                session_result.as_object_mut().expect("json object").insert(
                    "client_message_id".to_string(),
                    serde_json::Value::String(cmid.to_string()),
                );
            }
            let mut metadata_obj = serde_json::Map::new();
            if let Some(topic) = self.session_key.topic() {
                metadata_obj.insert(
                    "topic".to_string(),
                    serde_json::Value::String(topic.to_string()),
                );
            }
            metadata_obj.insert(
                "_history_persisted".to_string(),
                serde_json::Value::Bool(true),
            );
            metadata_obj.insert("_session_result".to_string(), session_result);
            // M8.10 PR #2: tag the user-message session_result emission with
            // thread_id so the API channel can stamp it on subsequent
            // wire events for this turn.
            if let Some(cmid) = client_message_id.as_deref() {
                metadata_obj.insert(
                    "thread_id".to_string(),
                    serde_json::Value::String(cmid.to_string()),
                );
            }

            let _ = send_outbound_with_timeout(
                &self.session_key,
                &self.out_tx,
                OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: String::new(),
                    reply_to: None,
                    media: vec![],
                    metadata: serde_json::Value::Object(metadata_obj),
                },
                "user_message_session_result_forced_background",
            )
            .await;
        }
        let ack_content = workflow_ack;
        let persisted = persist_assistant_message(
            &self.session_handle,
            &self.session_key,
            &self.data_dir,
            ack_content.clone(),
            vec![],
            client_message_id.clone(),
        )
        .await;

        // M8.10 PR #2: tag the forced-background ack and the trailing
        // _completion with the user's cmid so the SSE events the API
        // channel emits carry thread_id back to the web client. Same
        // events as the speculative path — just a different thread.
        let mut ack_metadata = serde_json::json!({
            "_history_persisted": persisted,
            "spawn_output": spawn_result.output,
        });
        if let Some(ref tid) = client_message_id {
            if let Some(map) = ack_metadata.as_object_mut() {
                map.insert(
                    "thread_id".to_string(),
                    serde_json::Value::String(tid.clone()),
                );
            }
        }
        let _ = self
            .out_tx
            .send(OutboundMessage {
                channel: self.channel.clone(),
                chat_id: self.chat_id.clone(),
                content: ack_content,
                reply_to,
                media: vec![],
                metadata: ack_metadata,
            })
            .await;

        if self.channel == "api" {
            let bg_tasks = tool_registry
                .supervisor()
                .get_tasks_for_session(&self.session_key.to_string())
                .into_iter()
                .filter(|task| task.status.is_active())
                .map(|task| sanitize_task_for_response(&self.data_dir, &task))
                .collect::<Vec<_>>();

            let mut completion_metadata = serde_json::json!({
                "_completion": true,
                "has_bg_tasks": !bg_tasks.is_empty(),
                "bg_tasks": bg_tasks,
            });
            if let Some(ref tid) = client_message_id {
                if let Some(map) = completion_metadata.as_object_mut() {
                    map.insert(
                        "thread_id".to_string(),
                        serde_json::Value::String(tid.clone()),
                    );
                }
            }
            let _ = self
                .out_tx
                .send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: String::new(),
                    reply_to: None,
                    media: vec![],
                    metadata: completion_metadata,
                })
                .await;
        }

        self.emit_turn_end_hook(persisted_user_content).await;

        true
    }

    /// Speculative processing: runs the LLM call but monitors the inbox.
    /// If the call exceeds 2× responsiveness baseline and a new user message
    /// arrives, the new message gets a quick LLM response via the adaptive
    /// router (no tools, lightweight) while the original call continues.
    /// Both results are delivered to the user.
    async fn process_inbound_speculative(
        &mut self,
        inbound: InboundMessage,
        image_media: Vec<String>,
        attachment_media: Vec<String>,
        attachment_prompt: Option<String>,
    ) {
        // Reset overflow cancellation from any prior command handling (#21).
        self.overflow_cancelled.store(false, Ordering::Release);

        // Capture the platform message ID for reply threading
        let inbound_message_id = inbound.message_id.clone();

        let patience = self
            .responsiveness
            .baseline()
            .map(|b| (b * 2).max(Duration::from_secs(10)))
            .unwrap_or(Duration::from_secs(30));
        debug!(
            session = %self.session_key,
            patience_ms = patience.as_millis(),
            baseline_ms = ?self.responsiveness.baseline().map(|b| b.as_millis()),
            samples = self.responsiveness.sample_count(),
            "speculative: entering concurrent processing"
        );

        let persisted_user_content =
            Self::persisted_user_content(&inbound, &image_media, &attachment_media);

        // ── Setup (needs &mut self briefly for permit + reporter) ────────

        let _permit = match self.semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => return,
        };

        if self
            .maybe_start_forced_background_workflow(
                &inbound,
                &image_media,
                &attachment_media,
                attachment_prompt.as_deref(),
                &persisted_user_content,
                inbound_message_id.clone(),
            )
            .await
        {
            self.cancelled.store(false, Ordering::Release);
            return;
        }

        let max_history = self.max_history.load(Ordering::Acquire);

        // Save the primary user message to session history BEFORE spawning
        // so overflow reads see it in context (chronological ordering).
        // Persist BOTH image_media and attachment_media so future turns can
        // re-reference uploaded audio/files. Without this, attachments only
        // survived as TurnAttachmentContext for the current turn.
        let client_message_id = inbound_client_message_id(&inbound);
        let persisted_user_content_for_event = persisted_user_content.clone();
        let user_media_for_event = image_media.clone();
        let user_msg = Message {
            role: MessageRole::User,
            content: persisted_user_content,
            media: image_media
                .iter()
                .chain(attachment_media.iter())
                .cloned()
                .collect(),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: client_message_id.clone(),
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        let user_msg_timestamp = user_msg.timestamp;
        let user_seq = {
            let mut handle = self.session_handle.lock().await;
            // Auto-generate summary from first user message
            {
                let session = handle.get_or_create();
                if session.summary.is_none() && !inbound.content.trim().is_empty() {
                    let summary: String = inbound.content.chars().take(100).collect();
                    session.summary = Some(summary);
                }
            }
            handle.add_message_with_seq(user_msg).await.ok()
        };

        // The web client sorts by Message.timestamp (timestamp-primary
        // comparator) so optimistic bubbles slot in chronological order
        // without needing a server seq round-trip. Seq is still captured for
        // ledger integrity.
        let _ = user_seq;
        let _ = user_msg_timestamp;
        let _ = persisted_user_content_for_event;
        let _ = user_media_for_event;

        // Get conversation history (now includes the user message we just saved)
        let history: Vec<Message> = {
            let handle = self.session_handle.lock().await;
            handle.get_history(max_history).to_vec()
        };

        // Token tracker for status indicator
        let token_tracker = Arc::new(TokenTracker::new());

        // Start status indicator
        let status_handle = self.status_indicator.as_ref().map(|si| {
            let voice_transcript = inbound
                .metadata
                .get("voice_transcript")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            si.start(
                self.chat_id.clone(),
                &inbound.content,
                Arc::clone(&token_tracker),
                voice_transcript,
                &self.user_status_config,
                self.sender_user_id.clone(),
            )
        });

        // Set up progressive streaming reporter.
        //
        // M8.10 PR #2: bind the user message's `client_message_id` to the
        // reporter so every emitted SSE payload (token, tool_start, ...)
        // carries `thread_id`. Speculative overflow + forced-background paths
        // construct their own reporter with their own cmid below — the same
        // event types flow through, just tagged with a different thread_id.
        let (stream_tx, stream_rx) = tokio::sync::mpsc::unbounded_channel();
        let reporter = Arc::new(
            crate::stream_reporter::ChannelStreamReporter::new(stream_tx.clone())
                .with_thread_id(client_message_id.clone()),
        );
        self.agent.set_reporter(reporter);

        // Wire adaptive router status callback to forward through the stream channel.
        // This lets failover events inside chat_stream() surface as LlmStatus messages.
        if let Some(ref router) = self.adaptive_router {
            let status_tx = stream_tx.clone();
            router.set_status_callback(Some(Arc::new(move |message: String| {
                let _ = status_tx
                    .send(crate::stream_reporter::StreamProgressEvent::LlmStatus { message });
            })));
        }

        // Drop the original stream_tx — the reporter and callback each hold their
        // own clones.  If we keep this alive, the stream forwarder will never see
        // channel-closed and the await at the end of this function deadlocks.
        drop(stream_tx);

        // Set provider layer on the status composer
        if let Some(ref handle) = status_handle {
            handle.set_provider(self.agent.provider_name(), self.agent.model_id());
        }

        // Spawn stream forwarder task (only for channels that support editing)
        let stream_forwarder = if let Some(ref si) = self.status_indicator {
            let channel = Arc::clone(si.channel());
            if channel.supports_edit() {
                let cancel_status = status_handle.as_ref().map(|h| Arc::clone(&h.cancelled));
                let status_msg_id = status_handle.as_ref().map(|h| Arc::clone(&h.status_msg_id));
                let op_updater = status_handle.as_ref().map(|h| h.operation_updater());
                Some(tokio::spawn(crate::stream_reporter::run_stream_forwarder(
                    stream_rx,
                    channel,
                    self.chat_id.clone(),
                    cancel_status,
                    status_msg_id,
                    Arc::clone(&self.active_sessions),
                    self.session_key.clone(),
                    self.sender_user_id.clone(),
                    op_updater,
                    // #649 follow-up (rapid-fire): forward THIS turn's
                    // cmid so the forwarder stamps every `send_with_id` /
                    // `edit_message` outbound with it. Concurrent overflow
                    // turns each get their OWN forwarder + their OWN
                    // cmid — under rapid-fire 5 turns that prevents the
                    // shared sticky map from collapsing them onto one
                    // bubble.
                    client_message_id.clone(),
                )))
            } else {
                drop(stream_rx);
                None
            }
        } else {
            drop(stream_rx);
            None
        };

        // ── Spawn agent call as a separate task (Arc<Agent>, no &mut self) ──

        let agent = Arc::clone(&self.agent);
        let content = inbound.content.clone();
        let media = image_media;
        let attachments = self.build_turn_attachment_context(attachment_media, attachment_prompt);
        let tracker = Arc::clone(&token_tracker);
        let session_timeout = self.session_timeout;

        // The agent receives the history snapshot (which includes the user
        // message we saved above). The agent will prepend its own system
        // prompt and user message internally — we'll deduplicate on save.
        // Note: we pass the history WITHOUT the user message we just saved,
        // because process_message_tracked adds a user message itself.
        // The pre-saved user message ensures overflow calls see it in history.
        let history_for_agent: Vec<Message> = if !history.is_empty() {
            // Strip the last message (the user msg we just saved) since the
            // agent's process_message_inner will re-add it.
            history[..history.len() - 1].to_vec()
        } else {
            vec![]
        };

        // Snapshot for overflow tasks: conversation context BEFORE the
        // primary task, EXCLUDING the primary user message.  Overflow needs
        // identity, preferences, and prior exchanges, but must NOT see the
        // primary question — otherwise the LLM re-answers it alongside the
        // overflow question.  Same base as history_for_agent (primary user
        // message stripped).
        let overflow_history = history_for_agent.clone();

        let mut agent_task = tokio::spawn(async move {
            let start = Instant::now();
            let result = tokio::time::timeout(
                session_timeout,
                agent.process_message_tracked_with_attachments(
                    &content,
                    &history_for_agent,
                    media,
                    attachments,
                    &tracker,
                ),
            )
            .await;
            eprintln!(
                "[DEBUG] agent_task finished in {}ms, ok={}",
                start.elapsed().as_millis(),
                result.is_ok()
            );
            (result, start.elapsed())
        });

        // ── Select loop: poll inbox while agent runs ────────────────────

        let started = Instant::now();
        let mut overflow_served = false;
        let mut overflow_commands: Vec<InboundMessage> = Vec::new();

        let (agent_result, llm_latency) = loop {
            tokio::select! {
                // Agent task completed
                join_result = &mut agent_task => {
                    match join_result {
                        Ok(pair) => break pair,
                        Err(e) => {
                            warn!(session = %self.session_key, error = %e, "agent task panicked");
                            self.send_reply("Internal error during processing.").await;
                            // Clean up reporter + status + callback
                            self.agent.set_reporter(Arc::new(octos_agent::SilentReporter));
                            if let Some(ref router) = self.adaptive_router {
                                router.set_status_callback(None);
                            }
                            if let Some(handle) = status_handle {
                                handle.stop().await;
                            }
                            return;
                        }
                    }
                }
                // New message arrived in inbox
                msg = self.inbox.recv() => {
                    match msg {
                        Some(ActorMessage::Inbound {
                            message,
                            image_media: _,
                            attachment_media: _,
                            attachment_prompt: _,
                        }) => {
                            if octos_core::is_abort_trigger(&message.content) {
                                self.cancelled.store(true, Ordering::Release);
                                self.send_reply(octos_core::abort_response(&message.content)).await;
                                continue;
                            }
                            // Check if this is a slash command — handle inline
                            // instead of spawning an overflow agent.
                            if message.content.trim().starts_with('/') {
                                overflow_commands.push(message);
                                continue;
                            }
                            let elapsed = started.elapsed();

                            if self.queue_mode == QueueMode::Interrupt {
                                // Interrupt mode: abort the primary agent task
                                // so the new message can be processed immediately.
                                info!(
                                    session = %self.session_key,
                                    elapsed_ms = elapsed.as_millis(),
                                    "interrupt: aborting primary task for new message"
                                );
                                agent_task.abort();
                                self.cancelled.store(true, Ordering::Release);

                                // Process the interrupting message as overflow
                                // (same as speculative, but the primary is now dead)
                                self.serve_overflow(&message, &overflow_history);
                                overflow_served = true;
                                continue;
                            }

                            info!(
                                session = %self.session_key,
                                elapsed_ms = elapsed.as_millis(),
                                patience_ms = patience.as_millis(),
                                "speculative: serving overflow message"
                            );
                            // Always spawn — the user sent a new message while
                            // the primary is running, so it needs processing.
                            self.serve_overflow(&message, &overflow_history);
                            overflow_served = true;
                        }
                        Some(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                            kind,
                            media,
                            originating_thread_id,
                            ack,
                        }) => {
                            let persisted = self
                                .handle_background_result(
                                    &task_label,
                                    &content,
                                    kind,
                                    media,
                                    originating_thread_id,
                                )
                                .await;
                            if let Some(ack) = ack {
                                let _ = ack.send(persisted);
                            }
                        }
                        Some(ActorMessage::TaskStatusChanged { task_json }) => {
                            let _ = self.out_tx.send(octos_core::OutboundMessage {
                                channel: self.channel.clone(),
                                chat_id: self.chat_id.clone(),
                                content: String::new(),
                                reply_to: None,
                                media: vec![],
                                metadata: serde_json::json!({
                                    "topic": self.session_key.topic(),
                                    "_task_status": task_json
                                }),
                            }).await;
                        }
                        Some(ActorMessage::RecoveryHint { task_id, tool_name, .. }) => {
                            // Speculative-overflow context: the primary
                            // turn is already running. Reserve the slot
                            // (so we don't try again later) and drop the
                            // hint — the failure will be visible via
                            // TaskStatusChanged.
                            debug!(
                                session = %self.session_key,
                                task_id,
                                tool_name,
                                "dropping recovery hint during speculative overflow"
                            );
                            self.claim_recovery_slot(&task_id);
                        }
                        Some(ActorMessage::Cancel) => {
                            self.cancelled.store(true, Ordering::Release);
                        }
                        None => {
                            // All senders dropped — actor shutting down
                            self.agent.set_reporter(Arc::new(octos_agent::SilentReporter));
                            if let Some(ref router) = self.adaptive_router {
                                router.set_status_callback(None);
                            }
                            if let Some(handle) = status_handle {
                                handle.stop().await;
                            }
                            return;
                        }
                    }
                }
            }
        };

        // ── Post-processing (back to &mut self) ────────────────────────

        // Drop the semaphore permit before &mut self operations below.
        drop(_permit);

        // Review A F-015: flush the cross-turn persistent retry-bucket state
        // to its JSON sidecar so the next `process_message` call on this
        // session sees the accumulated buckets. The in-memory `Arc<Mutex<..>>`
        // has already been mutated by the agent loop's guard; we just need
        // to persist it before the next turn loads. Best-effort: if the
        // sidecar write fails we log and carry on.
        if let Some(ref retry_path) = self.retry_state_path {
            let snapshot = self
                .persistent_retry_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            save_retry_state(retry_path, &snapshot);
        }

        // Handle any slash commands that arrived during the select loop.
        // We deferred them to avoid &mut self borrow conflicts in tokio::select!.
        for cmd_msg in &overflow_commands {
            self.try_handle_command(cmd_msg).await;
        }
        // If any deferred commands were processed, cancel in-flight overflow
        // tasks so their responses don't preempt command replies (#21).
        if !overflow_commands.is_empty() {
            self.overflow_cancelled.store(true, Ordering::Release);
        }

        // Feed latency to responsiveness observer
        self.responsiveness.record(llm_latency);
        if self.responsiveness.should_activate() {
            warn!(
                session = %self.session_key,
                baseline_ms = ?self.responsiveness.baseline().map(|b| b.as_millis()),
                latency_ms = llm_latency.as_millis(),
                consecutive_slow = self.responsiveness.consecutive_slow_count(),
                "sustained latency degradation detected, activating auto-protection"
            );
            self.responsiveness.set_active(true);
            self.queue_mode = QueueMode::Speculative;
            if let Some(ref router) = self.adaptive_router {
                router.set_mode(AdaptiveMode::Hedge);
                let _ = self.out_tx.send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: "⚡ Detected slow responses. Enabling hedge racing + speculative queue — you won't be blocked.".to_string(),
                    reply_to: None,
                    media: vec![],
                    metadata: serde_json::json!({}),
                }).await;
            }
        } else if self.responsiveness.should_deactivate() {
            info!(session = %self.session_key, "provider recovered, reverting to normal mode");
            self.responsiveness.set_active(false);
            self.queue_mode = QueueMode::Followup;
            if let Some(ref router) = self.adaptive_router {
                router.set_mode(AdaptiveMode::Off);
            }
        }

        // Reset reporter to silent (drops stream_tx → forwarder finishes)
        self.agent
            .set_reporter(Arc::new(octos_agent::SilentReporter));

        // Clear adaptive router status callback (stream_tx is being dropped)
        if let Some(ref router) = self.adaptive_router {
            router.set_status_callback(None);
        }

        // Wait for stream forwarder — but NOT for API channel.
        // For API channel, the forwarder blocks on rx.recv() which requires
        // _completion to close the SSE sender. Since _completion is sent after
        // this function's match block, awaiting the forwarder here would deadlock.
        let stream_result = if self.channel == "api" {
            // Drop the forwarder handle — it will finish on its own when _completion
            // arrives and closes the SSE sender.
            drop(stream_forwarder);
            None
        } else if let Some(handle) = stream_forwarder {
            (handle.await).ok()
        } else {
            None
        };

        // Stop status indicator
        if let Some(handle) = status_handle {
            handle.stop().await;
        }

        // Handle agent result — save messages (skipping user msg, already saved)
        // and send reply
        let supervisor = self.agent.tool_registry().supervisor();
        let bg_tasks = supervisor.task_count();
        let all_tasks = supervisor.get_all_tasks();
        let had_bg_tasks = !all_tasks.is_empty(); // any task was spawned, even if completed
        let bg_task_details: Vec<_> = supervisor.get_active_tasks();
        if !all_tasks.is_empty() {
            for t in &all_tasks {
                info!(
                    session = %self.session_key,
                    task_id = %t.id,
                    tool = %t.tool_name,
                    status = ?t.status,
                    files = ?t.output_files,
                    error = ?t.error,
                    "task supervisor report"
                );
            }
        }
        let mut completion_meta = match &agent_result {
            Ok(Ok(cr)) => {
                info!(session = %self.session_key, messages = cr.messages.len(), content_len = cr.content.len(), bg_tasks, "agent completed, saving messages");
                let provider_metadata = cr.provider_metadata.clone();
                let model_label = provider_metadata
                    .as_ref()
                    .map(|meta| meta.display_label())
                    .unwrap_or_else(|| {
                        format!("{}/{}", self.agent.provider_name(), self.agent.model_id())
                    });
                let model_id = provider_metadata
                    .as_ref()
                    .map(|meta| meta.model.clone())
                    .or_else(|| {
                        let model = self.agent.model_id();
                        if model.is_empty() {
                            None
                        } else {
                            Some(model.to_string())
                        }
                    });
                let session_cost = model_id.as_deref().and_then(model_pricing).map(|pricing| {
                    pricing.cost(cr.token_usage.input_tokens, cr.token_usage.output_tokens)
                });
                // Bug 3 / W1.G4 cost panel — collect per-node cost rows that
                // tools (today: `run_pipeline`) surfaced through their
                // `ToolResult.structured_metadata` side-channel. Without this
                // accumulator the data was being silently dropped between
                // the tool boundary and the SSE `done` event, leaving the
                // dashboard's CostBreakdown panel data-blind in production.
                let all_node_costs = collect_node_costs(&cr.tool_results);
                let mut meta_obj = serde_json::json!({
                    "_completion": true,
                    "model": model_label,
                    "provider": provider_metadata.as_ref().map(|meta| meta.provider.clone()),
                    "model_id": model_id,
                    "endpoint": provider_metadata.as_ref().and_then(|meta| meta.endpoint.clone()),
                    "tokens_in": cr.token_usage.input_tokens,
                    "tokens_out": cr.token_usage.output_tokens,
                    "session_cost": session_cost,
                    "duration_s": llm_latency.as_secs_f64().round() as u64,
                    "has_bg_tasks": had_bg_tasks,
                    "bg_tasks": bg_task_details,
                });
                if !all_node_costs.is_empty() {
                    if let Some(map) = meta_obj.as_object_mut() {
                        map.insert(
                            "node_costs".to_string(),
                            serde_json::Value::Array(all_node_costs),
                        );
                    }
                }
                meta_obj
            }
            Ok(Err(e)) => {
                warn!(session = %self.session_key, error = %e, "agent returned error");
                serde_json::json!({"_completion": true, "has_bg_tasks": had_bg_tasks, "bg_tasks": bg_task_details})
            }
            Err(e) => {
                warn!(session = %self.session_key, error = %e, "agent timed out");
                serde_json::json!({"_completion": true, "has_bg_tasks": had_bg_tasks, "bg_tasks": bg_task_details})
            }
        };
        match agent_result {
            Ok(Ok(conv_response)) => {
                let final_content = finalize_assistant_content(
                    &self.session_key,
                    &self.user_workspace,
                    &conv_response.content,
                );
                // Save tool calls, tool results, and assistant reply to history.
                // Skip the first message (user msg) — we already saved it before
                // spawning to maintain chronological ordering.
                let mut assistant_committed_seq: Option<u64> = None;
                {
                    let mut handle = self.session_handle.lock().await;
                    let messages_to_save = if !conv_response.messages.is_empty()
                        && conv_response.messages[0].role == MessageRole::User
                    {
                        &conv_response.messages[1..]
                    } else {
                        &conv_response.messages
                    };
                    for msg in messages_to_save {
                        // Issue #740 fix: pre-stamp `thread_id` on Assistant /
                        // Tool messages produced inside the agent loop. The
                        // agent builds these with `thread_id: None` and on
                        // persist `add_message_with_seq` derives thread_id
                        // from the most-recent USER message in history. Under
                        // rapid-fire fast-burst (live-overflow-stress.spec
                        // `rapid-fire-five-fast`), Q2/Q3 user rows have already
                        // been persisted to the same JSONL by the speculative-
                        // overflow tasks before THIS primary turn finalises,
                        // so derivation picks Qn's cmid instead of Q1's —
                        // mis-binding Q1's reply under Q3's bubble on reload.
                        // Pre-stamping the originating turn's cmid here pins
                        // the persisted JSONL row to the correct thread, the
                        // same fix shape PR #739 applied to the M8.9 spawn_only
                        // recovery path.
                        let mut to_save = msg.clone();
                        if to_save.thread_id.is_none()
                            && matches!(to_save.role, MessageRole::Assistant | MessageRole::Tool)
                        {
                            if let Some(ref tid) = client_message_id {
                                if !tid.is_empty() {
                                    to_save.thread_id = Some(tid.clone());
                                }
                            }
                        }
                        if let Err(e) = handle.add_message(to_save).await {
                            warn!(session = %self.session_key, role = ?msg.role, error = %e, "failed to persist message");
                        }
                    }

                    // The agent's ConversationResponse puts the final assistant
                    // text in `content` but may not include it as a Message in
                    // `messages` (EndTurn returns early without appending).
                    // Persist it explicitly so session history is complete.
                    if !conv_response.content.is_empty() {
                        let assistant_msg = Message {
                            role: MessageRole::Assistant,
                            content: final_content.clone(),
                            media: vec![],
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: conv_response.reasoning_content.clone(),
                            client_message_id: None,
                            // Issue #740 fix: pre-stamp `thread_id` from the
                            // originating turn's cmid so the persisted JSONL
                            // row is pinned to the correct thread. Without
                            // this, `add_message_with_seq`'s "most recent
                            // user in history" derivation picks the LATEST
                            // user message — which under rapid-fire is a
                            // sibling overflow user, not THIS turn — and
                            // reload mis-pairs the assistant under the
                            // wrong bubble (live-overflow-stress mini3
                            // `rapid-fire-five-fast` evidence: 1+1=2 rendered
                            // under the 3+3 bubble). Mirrors PR #739's M8.9
                            // recovery-path fix for the foreground SSE path.
                            thread_id: client_message_id.clone(),
                            timestamp: chrono::Utc::now(),
                        };
                        // M8.10-A: capture the committed seq so the SSE `done`
                        // event can thread it back to the web client. The
                        // assistant timestamp is `Utc::now()` (newer than any
                        // tool message) so the post-sort position matches the
                        // append index returned here.
                        match handle.add_message_with_seq(assistant_msg).await {
                            Ok(seq) => {
                                assistant_committed_seq = u64::try_from(seq).ok();
                            }
                            Err(e) => {
                                warn!(session = %self.session_key, error = %e, "failed to persist assistant reply");
                            }
                        }
                    }

                    // Sort messages by timestamp to restore chronological order.
                    // During concurrent speculative overflow, overflow responses
                    // may have been inserted before the primary call's messages.
                    handle.sort_by_timestamp();
                    if let Err(e) = handle.rewrite().await {
                        warn!(session = %self.session_key, error = %e, "failed to rewrite session after sort");
                    }

                    // Compact if needed
                    if let Err(e) = crate::compaction::maybe_compact_handle(
                        &mut handle,
                        &*self.llm_for_compaction,
                    )
                    .await
                    {
                        warn!("session compaction failed: {e}");
                    }
                }

                // M8.10-A: thread the committed assistant seq into the
                // completion_meta so the SSE done event can carry it back to
                // the web client. Live-streamed bubbles use this to populate
                // their `historySeq` and stay in chronological order.
                if let Some(seq) = assistant_committed_seq {
                    if let Some(map) = completion_meta.as_object_mut() {
                        map.insert("committed_seq".to_string(), serde_json::Value::from(seq));
                    }
                }

                // Auto-deliver report files produced by the agent (e.g. from run_pipeline).
                // This ensures the file reaches the user's channel (Telegram, web, etc.)
                // without relying on the LLM to call send_file within its token budget.
                if conv_response.files_modified.is_empty() {
                    tracing::debug!(session = %self.session_key, "no files_modified in conv_response");
                } else {
                    tracing::info!(
                        session = %self.session_key,
                        files = ?conv_response.files_modified.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
                        "conv_response has files_modified"
                    );
                }
                for file in &conv_response.files_modified {
                    if file.extension().and_then(|e| e.to_str()) == Some("md") {
                        // Resolve relative paths to absolute so the file URL works
                        let abs_file = if file.is_relative() {
                            std::fs::canonicalize(file)
                                .or_else(|_| std::fs::canonicalize(self.data_dir.join(file)))
                                .unwrap_or_else(|_| file.clone())
                        } else {
                            file.clone()
                        };
                        info!(
                            session = %self.session_key,
                            file = %abs_file.display(),
                            channel = %self.channel,
                            chat_id = %self.chat_id,
                            "auto-delivering report file"
                        );
                        let file_msg = OutboundMessage {
                            channel: self.channel.clone(),
                            chat_id: self.chat_id.clone(),
                            content: String::new(),
                            reply_to: None,
                            media: vec![abs_file.to_string_lossy().into_owned()],
                            metadata: serde_json::json!({
                                "topic": self.session_key.topic()
                            }),
                        };
                        if let Err(e) = self.out_tx.send(file_msg).await {
                            warn!(session = %self.session_key, error = %e, "failed to auto-deliver report file");
                        }
                    }
                }

                // Send reply
                let content = strip_think_tags(&final_content);
                let is_cron = inbound.channel == "system" && inbound.sender_id == "cron";
                let is_silent = content.trim().is_empty()
                    || content.contains("[SILENT]")
                    || content.contains("[NO_CHANGE]");

                if !(is_cron && is_silent) {
                    let display_content = if content.trim().is_empty() && !is_cron {
                        tracing::warn!(session = %self.session_key, "LLM returned empty content, sending fallback");
                        "(The model returned an empty response. Please try again.)".to_string()
                    } else {
                        content
                            .trim_start()
                            .strip_prefix("[SILENT]")
                            .or_else(|| content.trim_start().strip_prefix("[NO_CHANGE]"))
                            .unwrap_or(&content)
                            .to_string()
                    };

                    // Prepend thinking content when show_thinking is enabled
                    let display_content = if self.user_status_config.show_thinking {
                        let prefix =
                            format_thinking_prefix(conv_response.reasoning_content.as_deref());
                        format!("{prefix}{display_content}")
                    } else {
                        display_content
                    };

                    // The legacy "⬆️ Earlier task completed:" prefix was
                    // dropped because users misread it — the wording sounded
                    // like a stray prior reply when it actually meant "I
                    // also processed your follow-up below in parallel." Tool
                    // chips and the message timeline already convey that
                    // without confusing boilerplate. The `overflow_served`
                    // flag stays in scope so a future UI surface can render
                    // a richer indicator if needed.
                    let _ = overflow_served;

                    // Append annotation as last line for non-API channels
                    let display_content = if self.channel != "api" {
                        if let Some(model) = completion_meta.get("model").and_then(|v| v.as_str()) {
                            let tok_in = completion_meta
                                .get("tokens_in")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let tok_out = completion_meta
                                .get("tokens_out")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let secs = completion_meta
                                .get("duration_s")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            format!(
                                "{display_content}\n\n{}",
                                format_annotation(model, tok_in, tok_out, secs)
                            )
                        } else {
                            display_content
                        }
                    } else {
                        display_content
                    };

                    // Skip streaming edit when session is inactive — let the
                    // reply go through proxy → pending buffer for later flush.
                    let session_active = self.is_active().await;
                    let streamed = if session_active {
                        if let Some(ref sr) = stream_result {
                            if let Some(ref mid) = sr.message_id {
                                if let Some(ref si) = self.status_indicator {
                                    let _ = si
                                        .channel()
                                        .finish_stream(&self.chat_id, mid, &display_content)
                                        .await;
                                }
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if !streamed {
                        // M8.10 PR #2: tag the assistant reply with the
                        // turn's thread_id so the API channel can stamp
                        // it onto the SSE `replace` event it emits.
                        let mut reply_metadata = serde_json::json!({});
                        if let Some(ref tid) = client_message_id {
                            if let Some(map) = reply_metadata.as_object_mut() {
                                map.insert(
                                    "thread_id".to_string(),
                                    serde_json::Value::String(tid.clone()),
                                );
                            }
                        }
                        let _ = self
                            .out_tx
                            .send(OutboundMessage {
                                channel: self.channel.clone(),
                                chat_id: self.chat_id.clone(),
                                content: display_content,
                                reply_to: inbound_message_id.clone(),
                                media: vec![],
                                metadata: reply_metadata,
                            })
                            .await;
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::error!(session = %self.session_key, error = %e, "agent processing failed");
                let content = format!("Error: {e}");
                let _ = persist_terminal_reply_and_fanout(
                    &self.session_handle,
                    &self.session_key,
                    &self.data_dir,
                    &self.out_tx,
                    &self.channel,
                    &self.chat_id,
                    inbound_message_id.clone(),
                    content,
                    vec![],
                    client_message_id.as_deref(),
                )
                .await;
            }
            Err(_) => {
                record_timeout("session_turn");
                tracing::error!(session = %self.session_key, "session processing timed out");
                let content = "Processing timed out. Please try again.".to_string();
                let _ = persist_terminal_reply_and_fanout(
                    &self.session_handle,
                    &self.session_key,
                    &self.data_dir,
                    &self.out_tx,
                    &self.channel,
                    &self.chat_id,
                    inbound_message_id.clone(),
                    content,
                    vec![],
                    client_message_id.as_deref(),
                )
                .await;
            }
        }

        self.snapshot_workspace_turn_if_needed(&inbound.content, inbound_message_id.clone())
            .await;
        self.emit_turn_end_hook(&inbound.content).await;

        // Reset per-session cancellation flag so the next message starts fresh.
        // This must happen AFTER the agent finishes, so it has had a chance to
        // observe the shutdown signal during its iteration loop.
        self.cancelled.store(false, Ordering::Release);

        // M8.10 PR #2: tag the completion event with this turn's thread_id
        // (= the user message's client_message_id) so ApiChannel can stamp
        // it onto the SSE `done` payload. Applied uniformly across success,
        // error, and timeout branches above.
        if let Some(ref tid) = client_message_id {
            if let Some(map) = completion_meta.as_object_mut() {
                map.insert(
                    "thread_id".to_string(),
                    serde_json::Value::String(tid.clone()),
                );
            }
        }

        // Send completion marker so the API channel can close the SSE stream.
        if self.channel == "api" {
            let _ = self
                .out_tx
                .send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: String::new(),
                    reply_to: None,
                    media: vec![],
                    metadata: completion_meta,
                })
                .await;
        }
    }

    /// Spawn a full agent task for an overflow message (with tools).
    /// The task runs concurrently with the primary agent call.
    /// Each overflow gets its own chat bubble (stream reporter + status
    /// indicator) so the user sees independent progress per message.
    fn serve_overflow(&self, msg: &InboundMessage, pre_primary_history: &[Message]) {
        // Check per-session overflow concurrency limit
        let current = self.active_overflow_tasks.load(Ordering::Acquire);
        if current >= MAX_OVERFLOW_TASKS {
            warn!(
                session = %self.session_key,
                active = current,
                limit = MAX_OVERFLOW_TASKS,
                "overflow concurrency limit reached, returning busy response"
            );
            let out_tx = self.out_tx.clone();
            let channel = self.channel.clone();
            let chat_id = self.chat_id.clone();
            let reply_to = msg.message_id.clone();
            tokio::spawn(async move {
                let _ = out_tx
                    .send(OutboundMessage {
                        channel,
                        chat_id,
                        content: "I'm currently handling several tasks. Please wait a moment and try again.".to_string(),
                        reply_to,
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            });
            return;
        }
        self.active_overflow_tasks.fetch_add(1, Ordering::Release);

        info!(
            session = %self.session_key,
            overflow_content_len = msg.content.len(),
            history_len = pre_primary_history.len(),
            active_overflow = current + 1,
            "speculative: spawning full agent task for overflow with own chat bubble"
        );

        // Clone everything needed for the spawned task
        let agent = Arc::clone(&self.agent);
        let session_handle = Arc::clone(&self.session_handle);
        let overflow_counter = Arc::clone(&self.active_overflow_tasks);
        let out_tx = self.out_tx.clone();
        let channel = self.channel.clone();
        let chat_id = self.chat_id.clone();
        let session_key = self.session_key.clone();
        let content = msg.content.clone();
        let overflow_reply_to = msg.message_id.clone();
        let session_timeout = self.session_timeout;
        let status_indicator = self.status_indicator.clone();
        let sender_user_id = self.sender_user_id.clone();
        let user_status_config = self.user_status_config.clone();
        let pre_primary_history_vec = pre_primary_history.to_vec();
        let pre_primary_assistant_count = pre_primary_history_vec
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant))
            .count();
        let max_history = self.max_history.load(Ordering::Acquire);
        let active_sessions = self.active_sessions.clone();
        let overflow_cancelled = Arc::clone(&self.overflow_cancelled);
        let user_workspace = self.user_workspace.clone();
        let data_dir = self.data_dir.clone();
        let overflow_client_message_id = inbound_client_message_id(msg);

        tokio::spawn(async move {
            // Save user message to history first so it survives even if the
            // primary turn or this overflow agent fails — preserves the user's
            // query in the session log no matter what.
            let user_msg_timestamp = chrono::Utc::now();
            let user_msg = Message {
                role: MessageRole::User,
                content: content.clone(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: overflow_client_message_id.clone(),
                thread_id: None,
                timestamp: user_msg_timestamp,
            };
            let user_seq_for_overflow = {
                let mut handle = session_handle.lock().await;
                handle.add_message_with_seq(user_msg).await.ok()
            };

            // Restore the overflow user-message session_result emission that
            // was removed by 14ac3f3a — without it the web client has no signal
            // that user message B has a response slot, so streaming tokens for
            // B's reply bind to A's bubble (or render nowhere). The
            // timestamp-primary comparator handles ORDERING client-side; this
            // session_result handles ROUTING server-side. The two are
            // complementary, not exclusive. See #616. Channel-side fanout
            // (api_channel.rs) only honours `_session_result` for the api
            // channel; non-api adapters (telegram/etc) ignore it harmlessly.
            if let Some(seq) = user_seq_for_overflow {
                let mut session_result = serde_json::json!({
                    "seq": seq,
                    "role": "user",
                    "content": content.clone(),
                    "timestamp": user_msg_timestamp.to_rfc3339(),
                    "media": Vec::<String>::new(),
                });
                if let Some(cmid) = overflow_client_message_id.as_deref() {
                    session_result.as_object_mut().expect("json object").insert(
                        "client_message_id".to_string(),
                        serde_json::Value::String(cmid.to_string()),
                    );
                }
                let mut metadata_obj = serde_json::Map::new();
                if let Some(topic) = session_key.topic() {
                    metadata_obj.insert(
                        "topic".to_string(),
                        serde_json::Value::String(topic.to_string()),
                    );
                }
                metadata_obj.insert(
                    "_history_persisted".to_string(),
                    serde_json::Value::Bool(true),
                );
                metadata_obj.insert("_session_result".to_string(), session_result);
                // M8.10 PR #2: tag the user-message session_result emission
                // with thread_id so any SSE event the API channel emits in
                // response (e.g. when this metadata path also wraps content
                // into a `replace`) carries the right per-cmid routing key.
                if let Some(cmid) = overflow_client_message_id.as_deref() {
                    metadata_obj.insert(
                        "thread_id".to_string(),
                        serde_json::Value::String(cmid.to_string()),
                    );
                }

                let _ = send_outbound_with_timeout(
                    &session_key,
                    &out_tx,
                    OutboundMessage {
                        channel: channel.clone(),
                        chat_id: chat_id.clone(),
                        content: String::new(),
                        reply_to: None,
                        media: vec![],
                        metadata: serde_json::Value::Object(metadata_obj),
                    },
                    "user_message_session_result_overflow",
                )
                .await;
            }

            // Refresh the history snapshot so the overflow LLM sees the
            // primary turn's assistant reply if it has already landed. The
            // pre_primary_history_vec snapshot was captured before the primary
            // agent even started, so it would otherwise miss any answer the
            // primary just produced (e.g. a weather lookup the user asked
            // about right before sending the overflow follow-up).
            //
            // Bounded wait: 2s is enough for typical primary turns to flush
            // their final message; long-running primaries fall through with
            // the original pre_primary_history snapshot to preserve the
            // pre-fix safety property (overflow never sees the primary user
            // message in isolation, which would tempt the LLM to re-answer
            // it alongside the overflow question).
            let fresh_snapshot = wait_for_primary_assistant_reply(
                &session_handle,
                max_history,
                pre_primary_assistant_count,
                Duration::from_millis(2_000),
                Duration::from_millis(100),
            )
            .await;
            let fresh_assistant_count = fresh_snapshot
                .iter()
                .filter(|m| matches!(m.role, MessageRole::Assistant))
                .count();
            let primary_assistant_landed = fresh_assistant_count > pre_primary_assistant_count;
            let history: Vec<Message> = if primary_assistant_landed {
                // Strip our just-saved overflow user message so
                // process_message_tracked doesn't double-add it. Match by
                // exact timestamp (we control both sides).
                fresh_snapshot
                    .into_iter()
                    .filter(|m| {
                        !(matches!(m.role, MessageRole::User) && m.timestamp == user_msg_timestamp)
                    })
                    .collect()
            } else {
                // Primary still mid-turn — fall back to the safe pre-primary
                // snapshot (no primary user msg, no primary assistant reply).
                pre_primary_history_vec
            };
            let tracker = Arc::new(TokenTracker::new());

            // ── Per-overflow status indicator (own "✦ Thinking..." message) ──
            let status_handle = status_indicator.as_ref().map(|si| {
                si.start(
                    chat_id.clone(),
                    &content,
                    Arc::clone(&tracker),
                    None,
                    &user_status_config,
                    sender_user_id.clone(),
                )
            });

            // ── Per-overflow stream reporter (own chat bubble) ──────────────
            //
            // M8.10 PR #2: tag every SSE payload emitted by this reporter
            // with the overflow user's cmid so the web client can route
            // streaming tokens to the right per-thread bubble. This is the
            // critical bit that makes overflow stop being a special case —
            // same code path, same events, just a different thread_id.
            let (stream_tx, stream_rx) = tokio::sync::mpsc::unbounded_channel();
            let overflow_reporter: Arc<dyn octos_agent::ProgressReporter> = Arc::new(
                crate::stream_reporter::ChannelStreamReporter::new(stream_tx)
                    .with_thread_id(overflow_client_message_id.clone()),
            );

            // Spawn stream forwarder — edits its OWN message, not the primary's
            let stream_forwarder = if let Some(ref si) = status_indicator {
                let fwd_channel = Arc::clone(si.channel());
                let cancel_status = status_handle.as_ref().map(|h| Arc::clone(&h.cancelled));
                let status_msg_id = status_handle.as_ref().map(|h| Arc::clone(&h.status_msg_id));
                let op_updater = status_handle.as_ref().map(|h| h.operation_updater());
                Some(tokio::spawn(crate::stream_reporter::run_stream_forwarder(
                    stream_rx,
                    fwd_channel,
                    chat_id.clone(),
                    cancel_status,
                    status_msg_id,
                    active_sessions.clone(),
                    session_key.clone(),
                    sender_user_id.clone(),
                    op_updater,
                    // #649 follow-up (rapid-fire): each overflow turn
                    // captures its OWN cmid up front so its stream
                    // forwarder stamps every outbound with it. Without
                    // this, 5 concurrent rapid-fire overflow forwarders
                    // fight over the shared sticky map and collapse onto
                    // the bubble of whichever turn arrived last.
                    overflow_client_message_id.clone(),
                )))
            } else {
                drop(stream_rx);
                None
            };

            // ── Run agent with task-local reporter override ─────────────────
            let reporter_for_scope = overflow_reporter.clone();
            let result = octos_agent::TASK_REPORTER
                .scope(reporter_for_scope, async {
                    tokio::time::timeout(
                        session_timeout,
                        agent.process_message_tracked(&content, &history, vec![], &tracker),
                    )
                    .await
                })
                .await;

            // Drop the reporter so the stream forwarder sees channel close
            drop(overflow_reporter);

            // Wait for stream forwarder to finish flushing
            let stream_result = if let Some(handle) = stream_forwarder {
                handle.await.ok()
            } else {
                None
            };

            // Stop status indicator (deletes the "✦ Thinking..." message)
            if let Some(handle) = status_handle {
                handle.stop().await;
            }

            // If a slash command was handled while this overflow task was
            // running, suppress the response so it doesn't preempt the
            // command reply (GitHub issue #21).
            if overflow_cancelled.load(Ordering::Acquire) {
                info!(
                    session = %session_key,
                    "overflow task cancelled by command, suppressing response"
                );
                if let Some(notice) =
                    snapshot_workspace_turn_for_path(&session_key, user_workspace.clone(), &content)
                        .await
                {
                    emit_workspace_snapshot_notice(
                        &out_tx,
                        &channel,
                        &chat_id,
                        overflow_reply_to.clone(),
                        sender_user_id.as_deref(),
                        notice,
                    )
                    .await;
                }
                // Still decrement and return — skip sending any reply.
                overflow_counter.fetch_sub(1, Ordering::Release);
                return;
            }

            match result {
                Ok(Ok(conv_response)) => {
                    let final_content = finalize_assistant_content(
                        &session_key,
                        &user_workspace,
                        &conv_response.content,
                    );
                    // Save ONLY the final assistant reply to session history.
                    // Intermediate tool_call/tool_result messages are NOT saved
                    // to avoid tool_call ID collisions when multiple overflow
                    // tasks run concurrently (e.g. two deep_search_0 IDs).
                    //
                    // Capture the committed seq + timestamp so the outbound
                    // fanout below can carry `_session_result` metadata. The
                    // ApiChannel routes that metadata through
                    // `broadcast_session_event` → watchers, which survives
                    // the primary turn's SSE stream completion. Without this,
                    // the overflow reply would only route through
                    // `pending[session_id]` — already removed when the
                    // primary turn completed — and would be silently dropped
                    // (FA-11 defect B).
                    let final_reply_timestamp = chrono::Utc::now();
                    let final_reply = Message {
                        role: MessageRole::Assistant,
                        content: final_content.clone(),
                        media: vec![],
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: conv_response.reasoning_content.clone(),
                        client_message_id: None,
                        // Issue #740 fix: pre-stamp `thread_id` with the
                        // overflow user's own cmid. Without this, when
                        // multiple rapid-fire overflow tasks finalise in
                        // an out-of-order sequence (e.g. Q2's reply lands
                        // after Q5's user message has been persisted),
                        // `add_message_with_seq`'s derivation fallback
                        // picks the latest user (Q5) instead of THIS
                        // overflow's originating user (Q2), and the
                        // persisted JSONL row mis-binds the reply under
                        // Q5's bubble on reload. Mirrors PR #739's
                        // BackgroundResult fix for the speculative-
                        // overflow code path.
                        thread_id: overflow_client_message_id.clone(),
                        timestamp: final_reply_timestamp,
                    };
                    let committed_seq = {
                        let mut handle = session_handle.lock().await;
                        handle
                            .add_message_with_seq(final_reply)
                            .await
                            .map_err(|error| {
                                warn!(
                                    session = %session_key,
                                    error = %error,
                                    "failed to persist overflow assistant message"
                                );
                                error
                            })
                            .ok()
                    };

                    let reply = strip_think_tags(&final_content);
                    // Prepend thinking content when show_thinking is enabled
                    let reply = if user_status_config.show_thinking {
                        let prefix =
                            format_thinking_prefix(conv_response.reasoning_content.as_deref());
                        format!("{prefix}{reply}")
                    } else {
                        reply
                    };
                    // Check session activity — if inactive, skip streaming edit
                    // so the reply goes through proxy → pending buffer.
                    let session_active = {
                        let my_topic = session_key.topic().unwrap_or("");
                        let base_key = session_key.base_key();
                        let active_topic = active_sessions
                            .read()
                            .await
                            .get_active_topic(base_key)
                            .to_string();
                        my_topic == active_topic
                    };
                    let already_streamed = session_active
                        && stream_result
                            .as_ref()
                            .is_some_and(|sr| sr.message_id.is_some());

                    // FA-12 defect C: `already_streamed` is an unreliable
                    // "content already delivered" signal for ApiChannel —
                    // its `send_with_id` always returns `Some("sse-{chat_id}")`
                    // so the first stream_forwarder flush marks the overflow
                    // as "streamed", even if subsequent chunks silently no-op
                    // because `pending[chat_id]` was removed by the primary
                    // turn's `_completion`. Decouple the durable metadata
                    // emission from the user-facing content rendering: when
                    // we have a committed seq, always emit `_session_result`
                    // metadata so `ApiChannel::send` routes via
                    // `broadcast_session_event` → watchers (the durable
                    // fanout that survives primary-turn completion). When
                    // the channel already rendered the content inline, emit
                    // with empty body so non-API channels don't produce a
                    // duplicate bubble and the web side doesn't double-render.
                    let have_durable_metadata = committed_seq.is_some();
                    let should_emit =
                        !reply.trim().is_empty() && (have_durable_metadata || !already_streamed);

                    if should_emit {
                        let mut metadata = serde_json::Map::new();
                        metadata.insert(
                            "_history_persisted".to_string(),
                            serde_json::Value::Bool(committed_seq.is_some()),
                        );
                        if let Some(topic) = session_key.topic() {
                            metadata.insert("topic".to_string(), serde_json::Value::from(topic));
                        }
                        if let Some(seq) = committed_seq {
                            metadata.insert(
                                "_session_result".to_string(),
                                serde_json::json!({
                                    "seq": seq,
                                    "role": "assistant",
                                    "content": reply.clone(),
                                    "timestamp": final_reply_timestamp.to_rfc3339(),
                                    "media": Vec::<String>::new(),
                                    "response_to_client_message_id": overflow_reply_to.clone(),
                                }),
                            );
                        }
                        let outbound_content = if already_streamed {
                            String::new()
                        } else {
                            reply
                        };
                        // M8.10 PR #2: tag the overflow assistant reply
                        // with the overflow user's cmid so any wire events
                        // ApiChannel emits (replace, file, …) carry the
                        // correct thread_id. The done event for the overflow
                        // is the primary completion's done — that one is
                        // tagged with the primary's cmid, so an overflow
                        // can render before the primary completes.
                        if let Some(ref tid) = overflow_client_message_id {
                            metadata.insert(
                                "thread_id".to_string(),
                                serde_json::Value::String(tid.clone()),
                            );
                        }
                        let _ = out_tx
                            .send(OutboundMessage {
                                channel: channel.clone(),
                                chat_id: chat_id.clone(),
                                content: outbound_content,
                                reply_to: overflow_reply_to.clone(),
                                media: vec![],
                                metadata: serde_json::Value::Object(metadata),
                            })
                            .await;
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!(session = %session_key, error = %e, "overflow agent task failed");
                    let content = format!("Error: {e}");
                    let _ = persist_terminal_reply_and_fanout(
                        &session_handle,
                        &session_key,
                        &data_dir,
                        &out_tx,
                        &channel,
                        &chat_id,
                        overflow_reply_to.clone(),
                        content,
                        vec![],
                        overflow_client_message_id.as_deref(),
                    )
                    .await;
                }
                Err(_) => {
                    record_timeout("overflow_turn");
                    let content = "Processing timed out.".to_string();
                    let _ = persist_terminal_reply_and_fanout(
                        &session_handle,
                        &session_key,
                        &data_dir,
                        &out_tx,
                        &channel,
                        &chat_id,
                        overflow_reply_to.clone(),
                        content,
                        vec![],
                        overflow_client_message_id.as_deref(),
                    )
                    .await;
                }
            }

            if let Some(notice) =
                snapshot_workspace_turn_for_path(&session_key, user_workspace, &content).await
            {
                emit_workspace_snapshot_notice(
                    &out_tx,
                    &channel,
                    &chat_id,
                    overflow_reply_to.clone(),
                    sender_user_id.as_deref(),
                    notice,
                )
                .await;
            }
            // Decrement active overflow counter
            overflow_counter.fetch_sub(1, Ordering::Release);
        });
    }

    async fn process_inbound(
        &mut self,
        inbound: InboundMessage,
        image_media: Vec<String>,
        attachment_media: Vec<String>,
        attachment_prompt: Option<String>,
    ) {
        // Capture the platform message ID for reply threading
        let inbound_message_id = inbound.message_id.clone();
        // M8.10 PR #2: capture the user's client_message_id so every
        // OutboundMessage we emit (assistant reply, _completion, errors)
        // carries `thread_id` metadata. The API channel reads it back to
        // tag SSE payloads with the right per-cmid thread.
        let client_message_id = inbound_client_message_id(&inbound);

        // Acquire concurrency permit
        let _permit = match self.semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => return, // semaphore closed
        };

        // M8.6 per-turn worktree-missing check: the spawn-time sanitize runs
        // exactly once when the actor is created and cached in
        // ActorRegistry. If the workspace dir is deleted out-of-band between
        // turns, the cached actor would otherwise serve a stale in-memory
        // transcript whose tool calls reference state that no longer exists.
        // Clear the transcript and recreate the workspace so the next LLM
        // call starts from a known-empty state.
        if !self.user_workspace.exists() {
            warn!(
                session = %self.session_key,
                path = %self.user_workspace.display(),
                "per-turn worktree check: workspace missing on disk — \
                 clearing in-memory transcript before next LLM call"
            );
            {
                let mut handle = self.session_handle.lock().await;
                handle.clear_messages_for_unsafe_resume();
            }
            if let Err(e) = std::fs::create_dir_all(&self.user_workspace) {
                warn!(
                    session = %self.session_key,
                    path = %self.user_workspace.display(),
                    "per-turn worktree check: failed to recreate workspace: {e}"
                );
            }
        }

        let persisted_user_content =
            Self::persisted_user_content(&inbound, &image_media, &attachment_media);

        // Get conversation history
        let max_history = self.max_history.load(Ordering::Acquire);
        let history: Vec<Message> = {
            let mut handle = self.session_handle.lock().await;
            let session = handle.get_or_create();
            session.get_history(max_history).to_vec()
        };

        if self
            .maybe_start_forced_background_workflow(
                &inbound,
                &image_media,
                &attachment_media,
                attachment_prompt.as_deref(),
                &persisted_user_content,
                inbound_message_id.clone(),
            )
            .await
        {
            self.cancelled.store(false, Ordering::Release);
            return;
        }

        // Token tracker for status indicator
        let token_tracker = Arc::new(TokenTracker::new());

        // Start status indicator
        let status_handle = self.status_indicator.as_ref().map(|si| {
            let voice_transcript = inbound
                .metadata
                .get("voice_transcript")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            si.start(
                self.chat_id.clone(),
                &inbound.content,
                Arc::clone(&token_tracker),
                voice_transcript,
                &self.user_status_config,
                self.sender_user_id.clone(),
            )
        });

        // Set up progressive streaming reporter if we have a channel.
        //
        // M8.10 follow-up (#636): bind the inbound's `client_message_id`
        // to the reporter (matching the speculative-overflow path at
        // line 4006) so SSE payloads from this serial-delivery path
        // also carry `thread_id`. Most callers of `process_inbound`
        // are non-API channels (telegram/etc) where cmid is None, but
        // the recovery-hint path here is also reached from the API
        // channel and must thread cmid through for parity.
        let (stream_tx, stream_rx) = tokio::sync::mpsc::unbounded_channel();
        let reporter = Arc::new(
            crate::stream_reporter::ChannelStreamReporter::new(stream_tx.clone())
                .with_thread_id(client_message_id.clone()),
        );
        self.agent.set_reporter(reporter);

        // Wire adaptive router status callback for failover notifications
        if let Some(ref router) = self.adaptive_router {
            let status_tx = stream_tx.clone();
            router.set_status_callback(Some(Arc::new(move |message: String| {
                let _ = status_tx
                    .send(crate::stream_reporter::StreamProgressEvent::LlmStatus { message });
            })));
        }

        // Drop the original stream_tx — clones live in reporter + callback.
        // Without this, the stream forwarder await deadlocks.
        drop(stream_tx);

        // Set provider layer on the status composer
        if let Some(ref handle) = status_handle {
            handle.set_provider(self.agent.provider_name(), self.agent.model_id());
        }

        // Spawn stream forwarder task — edits a channel message as text arrives.
        // Only for channels that support message editing/streaming (Discord,
        // Telegram, Feishu, WeCom bot). Channels without edit support (Slack,
        // etc.) skip streaming to avoid sending duplicate messages.
        let stream_forwarder = if let Some(ref si) = self.status_indicator {
            let channel = Arc::clone(si.channel());
            if channel.supports_edit() {
                let cancel_status = status_handle.as_ref().map(|h| Arc::clone(&h.cancelled));
                let status_msg_id = status_handle.as_ref().map(|h| Arc::clone(&h.status_msg_id));
                let op_updater = status_handle.as_ref().map(|h| h.operation_updater());
                Some(tokio::spawn(crate::stream_reporter::run_stream_forwarder(
                    stream_rx,
                    channel,
                    self.chat_id.clone(),
                    cancel_status,
                    status_msg_id,
                    Arc::clone(&self.active_sessions),
                    self.session_key.clone(),
                    self.sender_user_id.clone(),
                    op_updater,
                    // #649 follow-up (rapid-fire): forward this turn's
                    // cmid so streaming chunks stamp it on the wire.
                    client_message_id.clone(),
                )))
            } else {
                drop(stream_rx);
                None
            }
        } else {
            // No channel available — drop the receiver so events are discarded
            drop(stream_rx);
            None
        };

        // Process through agent (potentially long LLM call)
        let llm_start = Instant::now();
        let result = tokio::time::timeout(
            self.session_timeout,
            self.agent.process_message_tracked_with_attachments(
                &inbound.content,
                &history,
                image_media,
                self.build_turn_attachment_context(attachment_media, attachment_prompt),
                &token_tracker,
            ),
        )
        .await;
        let llm_latency = llm_start.elapsed();
        eprintln!(
            "[DEBUG] process_inbound: agent returned in {}ms, ok={}",
            llm_latency.as_millis(),
            result.is_ok()
        );

        // Feed latency to responsiveness observer
        self.responsiveness.record(llm_latency);
        if self.responsiveness.should_activate() {
            warn!(
                session = %self.session_key,
                baseline_ms = ?self.responsiveness.baseline().map(|b| b.as_millis()),
                latency_ms = llm_latency.as_millis(),
                consecutive_slow = self.responsiveness.consecutive_slow_count(),
                "sustained latency degradation detected, activating auto-protection"
            );
            self.responsiveness.set_active(true);
            // Escalate: hedge routing (race providers) + speculative queue (unblock for new messages)
            self.queue_mode = QueueMode::Speculative;
            if let Some(ref router) = self.adaptive_router {
                router.set_mode(AdaptiveMode::Hedge);
                let _ = self.out_tx.send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: "⚡ Detected slow responses. Enabling hedge racing + speculative queue — you won't be blocked.".to_string(),
                    reply_to: None,
                    media: vec![],
                    metadata: serde_json::json!({}),
                }).await;
            }
        } else if self.responsiveness.should_deactivate() {
            info!(session = %self.session_key, "provider recovered, reverting to normal mode");
            self.responsiveness.set_active(false);
            self.queue_mode = QueueMode::Followup;
            if let Some(ref router) = self.adaptive_router {
                router.set_mode(AdaptiveMode::Off);
            }
        }

        // Reset reporter to silent (drop the stream sender → forwarder will finish)
        self.agent
            .set_reporter(Arc::new(octos_agent::SilentReporter));

        // Clear adaptive router status callback
        if let Some(ref router) = self.adaptive_router {
            router.set_status_callback(None);
        }

        // Wait for stream forwarder to complete and get its result
        let stream_result = if let Some(handle) = stream_forwarder {
            (handle.await).ok()
        } else {
            None
        };

        // Stop status indicator (if stream forwarder didn't already cancel it)
        if let Some(handle) = status_handle {
            handle.stop().await;
        }

        // Capture annotation data before match moves result
        let annotation_data: Option<(String, u32, u32, u64)> = if let Ok(Ok(ref cr)) = result {
            Some((
                cr.provider_metadata
                    .as_ref()
                    .map(|meta| meta.display_label())
                    .unwrap_or_else(|| {
                        format!("{}/{}", self.agent.provider_name(), self.agent.model_id())
                    }),
                cr.token_usage.input_tokens,
                cr.token_usage.output_tokens,
                llm_latency.as_secs(),
            ))
        } else {
            None
        };

        match result {
            Ok(Ok(conv_response)) => {
                let final_content = finalize_assistant_content(
                    &self.session_key,
                    &self.user_workspace,
                    &conv_response.content,
                );
                // Save all messages from the agent (user msg, tool calls, tool
                // results, assistant replies) so the full context is preserved
                // for subsequent calls.
                {
                    let mut handle = self.session_handle.lock().await;
                    // Auto-generate summary from first user message
                    {
                        let session = handle.get_or_create();
                        if session.summary.is_none() && !inbound.content.trim().is_empty() {
                            let summary: String = inbound.content.chars().take(100).collect();
                            session.summary = Some(summary);
                        }
                    }

                    let mut persisted_user_message = false;
                    for msg in &conv_response.messages {
                        let message_to_save =
                            if !persisted_user_message && msg.role == MessageRole::User {
                                persisted_user_message = true;
                                let mut sanitized = msg.clone();
                                sanitized.content = persisted_user_content.clone();
                                // Issue #738 fix: stamp the inbound's
                                // `client_message_id` onto the persisted user
                                // Message. The agent's `process_message`
                                // builds the user Message with
                                // `client_message_id: None` so without this
                                // override, recovery turns (whose synthetic
                                // InboundMessage carries the originating cmid
                                // in metadata) lose the cmid before reaching
                                // the SessionHandle — leaving the eventual
                                // successful retry's deliverables stranded
                                // under an orphan thread_id with no DOM bubble.
                                if sanitized.client_message_id.is_none() {
                                    sanitized.client_message_id = client_message_id.clone();
                                }
                                sanitized
                            } else {
                                let mut to_save = msg.clone();
                                // Issue #740 fix: pre-stamp `thread_id` on
                                // Assistant / Tool messages so the persisted
                                // JSONL row is pinned to THIS turn's cmid
                                // rather than letting `add_message_with_seq`
                                // derive it from the most-recent user in
                                // history (which can be a sibling rapid-fire
                                // turn that landed in the JSONL between this
                                // turn's user persist and assistant persist).
                                if to_save.thread_id.is_none()
                                    && matches!(
                                        to_save.role,
                                        MessageRole::Assistant | MessageRole::Tool
                                    )
                                {
                                    if let Some(ref tid) = client_message_id {
                                        if !tid.is_empty() {
                                            to_save.thread_id = Some(tid.clone());
                                        }
                                    }
                                }
                                to_save
                            };
                        if let Err(e) = handle.add_message(message_to_save).await {
                            warn!(session = %self.session_key, role = ?msg.role, error = %e, "failed to persist message");
                        }
                    }

                    // The agent's ConversationResponse puts the final assistant
                    // text in `content` but may not include it as a Message in
                    // `messages` (EndTurn returns early without appending).
                    // Persist it explicitly so session history is complete.
                    if !conv_response.content.is_empty() {
                        let assistant_msg = Message {
                            role: MessageRole::Assistant,
                            content: final_content.clone(),
                            media: vec![],
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: conv_response.reasoning_content.clone(),
                            client_message_id: None,
                            // Issue #740 fix: pre-stamp `thread_id` from the
                            // originating turn's cmid so reload pairs the
                            // assistant under the correct user bubble.
                            // Sibling fix to PR #739's M8.9 recovery path.
                            thread_id: client_message_id.clone(),
                            timestamp: chrono::Utc::now(),
                        };
                        if let Err(e) = handle.add_message(assistant_msg).await {
                            warn!(session = %self.session_key, error = %e, "failed to persist assistant reply");
                        }
                    }

                    // Compact if needed
                    if let Err(e) = crate::compaction::maybe_compact_handle(
                        &mut handle,
                        &*self.llm_for_compaction,
                    )
                    .await
                    {
                        warn!("session compaction failed: {e}");
                    }
                }

                // Send reply — always goes to this actor's chat (no race!)
                let content = strip_think_tags(&final_content);

                let is_cron = inbound.channel == "system" && inbound.sender_id == "cron";
                let is_silent = content.trim().is_empty()
                    || content.contains("[SILENT]")
                    || content.contains("[NO_CHANGE]");

                if !(is_cron && is_silent) {
                    let display_content = if content.trim().is_empty() && !is_cron {
                        tracing::warn!(session = %self.session_key, "LLM returned empty content, sending fallback");
                        "(The model returned an empty response. Please try again.)".to_string()
                    } else {
                        content
                            .trim_start()
                            .strip_prefix("[SILENT]")
                            .or_else(|| content.trim_start().strip_prefix("[NO_CHANGE]"))
                            .unwrap_or(&content)
                            .to_string()
                    };

                    // Prepend thinking content when show_thinking is enabled
                    let display_content = if self.user_status_config.show_thinking {
                        let prefix =
                            format_thinking_prefix(conv_response.reasoning_content.as_deref());
                        format!("{prefix}{display_content}")
                    } else {
                        display_content
                    };

                    // Append annotation as last line for non-API channels
                    let display_content = if self.channel != "api" {
                        if let Some((ref model, tok_in, tok_out, secs)) = annotation_data {
                            format!(
                                "{display_content}\n\n{}",
                                format_annotation(model, tok_in as u64, tok_out as u64, secs)
                            )
                        } else {
                            display_content
                        }
                    } else {
                        display_content
                    };

                    // If stream forwarder already sent a message AND this session
                    // is active, do a final edit. When inactive, skip the edit so
                    // the reply goes through the proxy → pending buffer path.
                    let session_active = self.is_active().await;
                    let streamed = if session_active {
                        if let Some(ref sr) = stream_result {
                            if let Some(ref mid) = sr.message_id {
                                if let Some(ref si) = self.status_indicator {
                                    let _ = si
                                        .channel()
                                        .finish_stream(&self.chat_id, mid, &display_content)
                                        .await;
                                }
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if !streamed {
                        // M8.10 PR #2: tag the assistant reply with the
                        // turn's thread_id so the API channel can stamp
                        // it onto the SSE `replace` event it emits.
                        let mut reply_metadata = serde_json::json!({});
                        if let Some(ref tid) = client_message_id {
                            if let Some(map) = reply_metadata.as_object_mut() {
                                map.insert(
                                    "thread_id".to_string(),
                                    serde_json::Value::String(tid.clone()),
                                );
                            }
                        }
                        let _ = self
                            .out_tx
                            .send(OutboundMessage {
                                channel: self.channel.clone(),
                                chat_id: self.chat_id.clone(),
                                content: display_content,
                                reply_to: inbound_message_id.clone(),
                                media: vec![],
                                metadata: reply_metadata,
                            })
                            .await;
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::error!(session = %self.session_key, error = %e, "agent processing failed");
                let content = format!("Error: {e}");
                let _ = persist_terminal_reply_and_fanout(
                    &self.session_handle,
                    &self.session_key,
                    &self.data_dir,
                    &self.out_tx,
                    &self.channel,
                    &self.chat_id,
                    inbound_message_id.clone(),
                    content,
                    vec![],
                    client_message_id.as_deref(),
                )
                .await;
            }
            Err(_) => {
                record_timeout("session_turn");
                tracing::error!(session = %self.session_key, "session processing timed out");
                let content = "Processing timed out. Please try again.".to_string();
                let _ = persist_terminal_reply_and_fanout(
                    &self.session_handle,
                    &self.session_key,
                    &self.data_dir,
                    &self.out_tx,
                    &self.channel,
                    &self.chat_id,
                    inbound_message_id.clone(),
                    content,
                    vec![],
                    client_message_id.as_deref(),
                )
                .await;
            }
        }

        self.snapshot_workspace_turn_if_needed(&inbound.content, inbound_message_id.clone())
            .await;
        self.emit_turn_end_hook(&inbound.content).await;

        // Reset per-session cancellation flag so the next message starts fresh.
        self.cancelled.store(false, Ordering::Release);

        // Send completion marker so the API channel can close the SSE stream.
        if self.channel == "api" {
            // M8.10 PR #2: tag the completion with the turn's thread_id
            // so ApiChannel stamps it onto the SSE `done` payload.
            let mut completion_metadata = serde_json::json!({"_completion": true});
            if let Some(ref tid) = client_message_id {
                if let Some(map) = completion_metadata.as_object_mut() {
                    map.insert(
                        "thread_id".to_string(),
                        serde_json::Value::String(tid.clone()),
                    );
                }
            }
            let _ = self
                .out_tx
                .send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: String::new(),
                    reply_to: None,
                    media: vec![],
                    metadata: completion_metadata,
                })
                .await;
        }
    }
}

/// Strip `<think>...</think>` blocks that some models embed inline.
/// Collapses runs of 3+ newlines left behind to avoid blank gaps.
fn strip_think_tags(s: &str) -> String {
    let mut result = s.to_string();
    while let Some(start) = result.find("<think>") {
        if let Some(end) = result[start..].find("</think>") {
            result.replace_range(start..start + end + "</think>".len(), "");
        } else {
            result.truncate(start);
            break;
        }
    }
    result = strip_invoke_tags(&result);
    // Collapse runs of 3+ newlines (left behind after stripping) to double newline
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result.trim().to_string()
}

/// Strip inline XML-style tool invocation markup:
/// `<invoke name="tool">...</invoke>` and self-closing `<invoke ... />`.
fn strip_invoke_tags(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;

    loop {
        let Some(start) = rest.find("<invoke") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let from_tag = &rest[start..];

        let Some(open_end) = from_tag.find('>') else {
            out.push_str(from_tag);
            break;
        };
        let open_tag = &from_tag[..=open_end];
        let after_open = &from_tag[open_end + 1..];

        if open_tag.trim_end().ends_with("/>") {
            rest = after_open;
            continue;
        }

        if let Some(close_rel) = after_open.find("</invoke>") {
            rest = &after_open[close_rel + "</invoke>".len()..];
        } else {
            // Unclosed invoke tag: drop the remainder to avoid leaking tool markup.
            break;
        }
    }

    out
}

/// Format token count with K suffix for readability (e.g. 22173 → "22.2K").
fn fmt_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Format annotation line: model · tokens in/out · duration
fn format_annotation(model: &str, tok_in: u64, tok_out: u64, secs: u64) -> String {
    format!(
        "_{model} · {in_} in · {out_} out · {secs}s_",
        in_ = fmt_tokens(tok_in),
        out_ = fmt_tokens(tok_out),
    )
}

/// Format reasoning/thinking content for display, prepended to the response.
/// Truncates long reasoning to avoid flooding the channel.
fn format_thinking_prefix(reasoning: Option<&str>) -> String {
    const MAX_THINKING_LEN: usize = 1000;
    match reasoning {
        Some(r) if !r.trim().is_empty() => {
            let trimmed = r.trim();
            let display = if trimmed.chars().count() > MAX_THINKING_LEN {
                let truncated: String = trimmed.chars().take(MAX_THINKING_LEN).collect();
                format!("{truncated}...")
            } else {
                trimmed.to_string()
            };
            format!("💭 *Thinking:*\n{display}\n\n---\n\n")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use octos_agent::{HookConfig, HookEvent};
    use octos_llm::{AdaptiveConfig, ChatConfig, ChatResponse, StopReason, TokenUsage, ToolSpec};
    use std::sync::atomic::AtomicUsize;

    #[cfg(unix)]
    fn capture_hook(event: HookEvent, log_path: &std::path::Path) -> HookConfig {
        HookConfig {
            event,
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"payload=$(cat); printf "%s\n" "$payload" >> "$1""#.into(),
                "sh".into(),
                log_path.to_string_lossy().into_owned(),
            ],
            timeout_ms: 5000,
            tool_filter: vec![],
        }
    }

    #[test]
    fn test_strip_think_tags() {
        assert_eq!(strip_think_tags("hello"), "hello");
        assert_eq!(strip_think_tags("<think>hmm</think>hello"), "hello");
        assert_eq!(
            strip_think_tags("before<think>hmm</think>after"),
            "beforeafter"
        );
        assert_eq!(strip_think_tags("<think>unclosed"), "");
        assert_eq!(
            strip_think_tags("ok <invoke name=\"cron\">{\"action\":\"list\"}</invoke> done"),
            "ok  done"
        );
    }

    #[test]
    fn test_strip_invoke_tags_self_closing() {
        assert_eq!(
            strip_invoke_tags("a<invoke name=\"cron\" args='{}' />b"),
            "ab"
        );
    }

    /// Gap 3.2 — when a tool surfaced `node_costs` via
    /// `ToolResult.structured_metadata`, `process_inbound`'s metadata
    /// builder must concatenate every row across tool results so the
    /// SSE `done` event carries the per-node cost array. Tested through
    /// the same `collect_node_costs` helper `process_inbound` calls.
    #[test]
    fn collect_node_costs_concatenates_rows_from_multiple_tool_results() {
        let tool_results = vec![
            (
                "call_pipeline_1".to_string(),
                serde_json::json!({
                    "node_costs": [
                        {"node_id": "draft",  "tokens_in": 320, "tokens_out": 110, "actual_usd": 0.0008},
                        {"node_id": "refine", "tokens_in": 540, "tokens_out": 220, "actual_usd": 0.0032},
                    ]
                }),
            ),
            (
                "call_pipeline_2".to_string(),
                serde_json::json!({
                    "node_costs": [
                        {"node_id": "synthesize", "tokens_in": 720, "tokens_out": 410, "actual_usd": 0.0091}
                    ]
                }),
            ),
        ];

        let collected = collect_node_costs(&tool_results);
        assert_eq!(collected.len(), 3, "rows from both pipelines must merge");
        assert_eq!(
            collected[0].get("node_id").and_then(|v| v.as_str()),
            Some("draft")
        );
        assert_eq!(
            collected[2].get("node_id").and_then(|v| v.as_str()),
            Some("synthesize")
        );
    }

    /// When no tool produced cost rows, the helper returns an empty vector
    /// so the calling code can omit the `node_costs` key from the SSE
    /// payload entirely (legacy clients see byte-identical events).
    #[test]
    fn collect_node_costs_returns_empty_when_no_tool_surfaced_metadata() {
        let tool_results: Vec<(String, serde_json::Value)> = Vec::new();
        assert!(collect_node_costs(&tool_results).is_empty());

        let unrelated = vec![(
            "call_other_tool".to_string(),
            serde_json::json!({"some_other_key": "value"}),
        )];
        assert!(collect_node_costs(&unrelated).is_empty());
    }

    /// End-to-end shape — drop the helper output into the same
    /// `completion_meta` builder shape used by `process_inbound` and
    /// confirm the SSE payload carries `node_costs`.
    #[test]
    fn completion_meta_carries_node_costs_when_tool_results_have_metadata() {
        let tool_results = vec![(
            "call_pipeline_1".to_string(),
            serde_json::json!({
                "node_costs": [
                    {"node_id": "draft", "tokens_in": 320, "tokens_out": 110, "actual_usd": 0.0008}
                ]
            }),
        )];

        let collected = collect_node_costs(&tool_results);
        let mut meta = serde_json::json!({
            "_completion": true,
            "tokens_in": 320,
            "tokens_out": 110,
        });
        if !collected.is_empty() {
            meta.as_object_mut().unwrap().insert(
                "node_costs".to_string(),
                serde_json::Value::Array(collected),
            );
        }
        let arr = meta
            .get("node_costs")
            .and_then(|v| v.as_array())
            .expect("completion_meta must carry node_costs once a tool surfaced rows");
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0].get("node_id").and_then(|v| v.as_str()),
            Some("draft")
        );
    }

    #[test]
    fn test_resolve_builtin_slides_styles_dir_falls_back_to_root_profile() {
        let dir = tempfile::TempDir::new().unwrap();
        let octos_home = dir.path().join(".octos");
        let current_data = octos_home
            .join("profiles")
            .join("dspfac--newsbot")
            .join("data");
        let root_styles = octos_home
            .join("profiles")
            .join("dspfac")
            .join("data")
            .join("skills")
            .join("mofa-slides")
            .join("styles");

        std::fs::create_dir_all(&current_data).unwrap();
        std::fs::create_dir_all(&root_styles).unwrap();
        std::fs::write(root_styles.join("default.toml"), "name = 'default'\n").unwrap();

        let resolved = resolve_builtin_slides_styles_dir(&current_data).unwrap();

        assert_eq!(resolved, root_styles);
    }

    #[test]
    fn test_resolve_builtin_slides_styles_dir_does_not_use_unrelated_profile() {
        let dir = tempfile::TempDir::new().unwrap();
        let octos_home = dir.path().join(".octos");
        let current_data = octos_home
            .join("profiles")
            .join("dspfac--newsbot")
            .join("data");
        let unrelated_styles = octos_home
            .join("profiles")
            .join("someone-else")
            .join("data")
            .join("skills")
            .join("mofa-slides")
            .join("styles");

        std::fs::create_dir_all(&current_data).unwrap();
        std::fs::create_dir_all(&unrelated_styles).unwrap();
        std::fs::write(unrelated_styles.join("default.toml"), "name = 'default'\n").unwrap();

        let resolved = resolve_builtin_slides_styles_dir(&current_data);

        assert!(resolved.is_none());
    }

    #[test]
    fn finalize_assistant_content_appends_site_preview_url_when_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let session_key = SessionKey::with_profile_topic("dspfac", "api", "web-123", "site astro");
        let metadata = crate::project_templates::build_site_project_metadata(
            "dspfac",
            "web-123",
            "site astro",
            dir.path(),
        )
        .expect("site metadata");
        let project_dir = dir.path().join(&metadata.project_dir);
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("mofa-site-session.json"),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();

        let finalized = finalize_assistant_content(&session_key, dir.path(), "✅ Site rebuilt.");

        assert!(finalized.contains("✅ Site rebuilt."));
        assert!(finalized.contains(&metadata.preview_url));
    }

    #[test]
    fn finalize_assistant_content_keeps_existing_site_preview_url() {
        let dir = tempfile::TempDir::new().unwrap();
        let session_key = SessionKey::with_profile_topic("dspfac", "api", "web-123", "site astro");
        let metadata = crate::project_templates::build_site_project_metadata(
            "dspfac",
            "web-123",
            "site astro",
            dir.path(),
        )
        .expect("site metadata");
        let project_dir = dir.path().join(&metadata.project_dir);
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("mofa-site-session.json"),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();

        let original = format!("✅ Site rebuilt.\n\nPreview URL: {}", metadata.preview_url);
        let finalized = finalize_assistant_content(&session_key, dir.path(), &original);

        assert_eq!(finalized, original);
    }

    #[test]
    fn session_task_query_store_hides_absolute_output_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        let workspace = data_dir
            .join("users")
            .join("api%3Asession")
            .join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let output = workspace.join("voice.mp3");
        std::fs::write(&output, b"audio").unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_ledger_path = data_dir.join("tasks.jsonl");
        supervisor.enable_persistence(&task_ledger_path).unwrap();
        let task_id = supervisor.register_with_lineage(
            "fm_tts",
            "call-1",
            Some("api:session"),
            Some(task_ledger_path.to_str().unwrap()),
        );
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            octos_agent::TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );
        supervisor.mark_completed(&task_id, vec![output.to_string_lossy().to_string()]);

        let store = SessionTaskQueryStore::default();
        let session_key = SessionKey::new("api", "session");
        store.register(&session_key, &supervisor, &data_dir);

        let payload = store.query_json(&session_key.to_string());
        let tasks = payload.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["lifecycle_state"], "ready");
        assert_eq!(tasks[0]["runtime_state"], "completed");
        assert_eq!(tasks[0]["runtime_detail"], "send_file");
        let files = tasks[0]["output_files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        let handle = files[0].as_str().unwrap();
        assert!(handle.starts_with("pf/"));
        assert!(!handle.starts_with("/"));
        assert_eq!(tasks[0]["parent_session_key"], "api:session");
        assert!(
            tasks[0]["child_session_key"]
                .as_str()
                .unwrap()
                .starts_with("api:session#child-")
        );
        assert!(tasks[0]["task_ledger_path"].is_null());
    }

    #[test]
    fn session_task_query_store_exposes_parsed_workflow_runtime_detail() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        let workspace = data_dir
            .join("users")
            .join("api%3Asession")
            .join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_ledger_path = data_dir.join("tasks.jsonl");
        supervisor.enable_persistence(&task_ledger_path).unwrap();
        let task_id = supervisor.register_with_lineage(
            "podcast_generate",
            "call-1",
            Some("api:session"),
            Some(task_ledger_path.to_str().unwrap()),
        );
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            octos_agent::TaskRuntimeState::DeliveringOutputs,
            Some(
                serde_json::json!({
                    "workflow_kind": "research_podcast",
                    "current_phase": "deliver_result"
                })
                .to_string(),
            ),
        );
        supervisor.mark_completed(&task_id, vec![]);
        supervisor.mark_child_session_outcome(
            &task_id,
            octos_agent::task_supervisor::ChildSessionTerminalState::Completed,
            octos_agent::task_supervisor::ChildSessionJoinState::Joined,
        );

        let store = SessionTaskQueryStore::default();
        let session_key = SessionKey::new("api", "session");
        store.register(&session_key, &supervisor, &data_dir);

        let payload = store.query_json(&session_key.to_string());
        let tasks = payload.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["lifecycle_state"], "ready");
        assert_eq!(tasks[0]["runtime_state"], "completed");
        assert_eq!(tasks[0]["workflow_kind"], "research_podcast");
        assert_eq!(tasks[0]["current_phase"], "deliver_result");
        assert_eq!(
            tasks[0]["runtime_detail"]["workflow_kind"],
            "research_podcast"
        );
        assert_eq!(
            tasks[0]["runtime_detail"]["current_phase"],
            "deliver_result"
        );
        assert_eq!(tasks[0]["child_terminal_state"], "completed");
        assert_eq!(tasks[0]["child_join_state"], "joined");
        assert!(tasks[0]["child_failure_action"].is_null());
    }

    #[test]
    fn session_task_query_store_exposes_harness_progress_runtime_detail() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_ledger_path = data_dir.join("tasks.jsonl");
        supervisor.enable_persistence(&task_ledger_path).unwrap();
        let task_id = supervisor.register_with_lineage(
            "deep_search",
            "call-1",
            Some("api:session"),
            Some(task_ledger_path.to_str().unwrap()),
        );
        supervisor.mark_running(&task_id);
        let event = octos_agent::HarnessEvent::progress(
            "api:session",
            task_id.clone(),
            Some("deep_research"),
            "fetch",
            Some("Fetching 4 pages"),
            Some(0.4),
        );
        supervisor.apply_harness_event(&task_id, &event).unwrap();

        let store = SessionTaskQueryStore::default();
        let session_key = SessionKey::new("api", "session");
        store.register(&session_key, &supervisor, &data_dir);

        let payload = store.query_json(&session_key.to_string());
        let tasks = payload.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], task_id);
        assert_eq!(tasks[0]["session_key"], "api:session");
        assert_eq!(tasks[0]["workflow_kind"], "deep_research");
        assert_eq!(tasks[0]["current_phase"], "fetch");
        assert_eq!(tasks[0]["runtime_detail"]["session_id"], "api:session");
        assert_eq!(tasks[0]["runtime_detail"]["task_id"], task_id);
        assert_eq!(
            tasks[0]["runtime_detail"]["progress_message"],
            "Fetching 4 pages"
        );
        assert_eq!(tasks[0]["runtime_detail"]["progress"], 0.4);
    }

    #[test]
    fn session_task_query_store_projects_verifying_lifecycle_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_ledger_path = data_dir.join("tasks.jsonl");
        supervisor.enable_persistence(&task_ledger_path).unwrap();
        let task_id = supervisor.register_with_lineage(
            "site_build",
            "call-1",
            Some("api:session"),
            Some(task_ledger_path.to_str().unwrap()),
        );
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            octos_agent::TaskRuntimeState::VerifyingOutputs,
            Some(
                serde_json::json!({
                    "workflow_kind": "site",
                    "current_phase": "verify_contract"
                })
                .to_string(),
            ),
        );

        let store = SessionTaskQueryStore::default();
        let session_key = SessionKey::new("api", "session");
        store.register(&session_key, &supervisor, &data_dir);

        let payload = store.query_json(&session_key.to_string());
        let tasks = payload.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["status"], "running");
        assert_eq!(tasks[0]["lifecycle_state"], "verifying");
        assert_eq!(tasks[0]["runtime_state"], "verifying_outputs");
        assert_eq!(tasks[0]["workflow_kind"], "site");
        assert_eq!(tasks[0]["current_phase"], "verify_contract");
        assert_eq!(tasks[0]["runtime_detail"]["workflow_kind"], "site");
        assert_eq!(
            tasks[0]["runtime_detail"]["current_phase"],
            "verify_contract"
        );
    }

    #[test]
    fn contract_owned_topics_require_serial_delivery() {
        assert!(topic_requires_serial_delivery(Some(
            "slides browser-acceptance"
        )));
        assert!(topic_requires_serial_delivery(Some("site")));
        assert!(topic_requires_serial_delivery(Some("site astro-demo")));
        assert!(!topic_requires_serial_delivery(Some("research")));
        assert!(!topic_requires_serial_delivery(None));
    }

    #[test]
    fn mark_child_session_failed_marks_owning_task_when_supervisor_registered() {
        // M8 fix-first item 8 (gap 3): when a child session refuses to
        // resume because its worktree is gone, SessionTaskQueryStore must
        // walk every registered supervisor, find the BackgroundTask
        // whose `child_session_key` matches, and call mark_failed on it.
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_ledger_path = data_dir.join("tasks.jsonl");
        supervisor.enable_persistence(&task_ledger_path).unwrap();

        // Register a parent task that spawns a child session — the
        // supervisor's `register_with_lineage` derives a deterministic
        // `child_session_key` from the parent + task id.
        let parent_session_key = SessionKey::new("api", "parent-session");
        let task_id = supervisor.register_with_lineage(
            "spawn",
            "call-1",
            Some(&parent_session_key.to_string()),
            Some(task_ledger_path.to_str().unwrap()),
        );
        supervisor.mark_running(&task_id);

        // Pull the derived child_session_key the supervisor recorded.
        let registered_task = supervisor.get_task(&task_id).expect("task tracked");
        let child_session_key = registered_task
            .child_session_key
            .clone()
            .expect("register_with_lineage derives a child key");

        // Register the supervisor in the query store as the parent
        // session would. The store now tracks a Weak<TaskSupervisor>
        // keyed by parent session key.
        let store = SessionTaskQueryStore::default();
        store.register(&parent_session_key, &supervisor, &data_dir);

        // ACT: simulate the child session refusing to resume.
        let was_marked = store.mark_child_session_failed(
            &child_session_key,
            "resume sanitize refused: worktree missing",
        );
        assert!(was_marked, "the parent task must be located by child key");

        // ASSERT: the task transitioned to Failed with the supplied error.
        let updated = supervisor.get_task(&task_id).expect("task still tracked");
        assert_eq!(
            updated.status,
            octos_agent::TaskStatus::Failed,
            "WorktreeMissing on a child session must mark the parent task failed"
        );
        assert!(
            updated
                .error
                .as_deref()
                .map(|e| e.contains("worktree missing"))
                .unwrap_or(false),
            "task error must carry the resume failure reason: {:?}",
            updated.error
        );
    }

    #[test]
    fn mark_child_session_failed_returns_false_when_no_task_matches() {
        // The store returns false when no registered supervisor owns a
        // task with the requested child_session_key. This guards against
        // false-positive marks on unrelated supervisors.
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        let parent_session_key = SessionKey::new("api", "parent-session");
        let store = SessionTaskQueryStore::default();
        store.register(&parent_session_key, &supervisor, &data_dir);

        let was_marked = store.mark_child_session_failed("api:other-session#child-zzz", "anything");
        assert!(
            !was_marked,
            "mark_child_session_failed must return false when no task matches"
        );
    }

    #[test]
    fn query_json_includes_descendant_session_tasks() {
        // Server-side bug fix: `/api/sessions/:id/tasks` previously
        // returned ONLY the parent session's tasks. When a workflow runs
        // `run_pipeline` in a CHILD session (parent spawns child via
        // spawn_only), that task was invisible from the parent view —
        // blocking UIs that cross-correlate the rendered tool_call_id
        // bubble with the actual run_pipeline task.
        //
        // After the fix, query_json walks the parent's session_key and
        // every reachable descendant (via each task's `child_session_key`)
        // breadth-first, returning a flat array carrying both sets. Each
        // entry's existing `session_key` field lets callers filter
        // parent-only when needed.
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Parent session: register a `spawn` task. The supervisor derives
        // a deterministic child_session_key the way the live spawn tool
        // would.
        let parent_supervisor = Arc::new(TaskSupervisor::new());
        let parent_ledger = data_dir.join("parent-tasks.jsonl");
        parent_supervisor
            .enable_persistence(&parent_ledger)
            .unwrap();
        let parent_session_key = SessionKey::new("api", "parent-session");
        let parent_task_id = parent_supervisor.register_with_lineage(
            "spawn",
            "call-spawn",
            Some(&parent_session_key.to_string()),
            Some(parent_ledger.to_str().unwrap()),
        );
        parent_supervisor.mark_running(&parent_task_id);

        // Pull the derived child session key the supervisor recorded.
        let parent_task = parent_supervisor
            .get_task(&parent_task_id)
            .expect("parent task tracked");
        let child_session_key_str = parent_task
            .child_session_key
            .clone()
            .expect("register_with_lineage derives a child key");
        let child_session_key = SessionKey(child_session_key_str.clone());

        // Child session: register its own supervisor with a `run_pipeline`
        // task (the workflow whose tool_call_id the UI wants to correlate
        // back from the parent).
        let child_supervisor = Arc::new(TaskSupervisor::new());
        let child_ledger = data_dir.join("child-tasks.jsonl");
        child_supervisor.enable_persistence(&child_ledger).unwrap();
        let child_task_id = child_supervisor.register_with_lineage(
            "run_pipeline",
            "call-pipeline",
            Some(&child_session_key_str),
            Some(child_ledger.to_str().unwrap()),
        );
        child_supervisor.mark_running(&child_task_id);

        // Both supervisors register against the shared store, the way
        // ActorRunner does at startup for each session it serves.
        let store = SessionTaskQueryStore::default();
        store.register(&parent_session_key, &parent_supervisor, &data_dir);
        store.register(&child_session_key, &child_supervisor, &data_dir);

        // ACT: query the parent. Both tasks should surface in one flat
        // array.
        let payload = store.query_json(&parent_session_key.to_string());
        let tasks = payload.as_array().expect("array response");
        assert_eq!(
            tasks.len(),
            2,
            "parent /tasks must surface its own task plus the child's run_pipeline task"
        );

        let parent_entry = tasks
            .iter()
            .find(|t| t["tool_name"] == "spawn")
            .expect("parent spawn task present");
        assert_eq!(parent_entry["session_key"], "api:parent-session");
        assert_eq!(
            parent_entry["child_session_key"], child_session_key_str,
            "parent task carries its derived child_session_key"
        );

        let child_entry = tasks
            .iter()
            .find(|t| t["tool_name"] == "run_pipeline")
            .expect("child run_pipeline task surfaces from parent view");
        assert_eq!(child_entry["session_key"], child_session_key_str);
        assert_eq!(child_entry["tool_call_id"], "call-pipeline");
    }

    #[test]
    fn query_json_walks_multi_level_descendants_without_cycling() {
        // The traversal must follow chains deeper than one level
        // (parent -> spawn -> run_pipeline can go 3+ levels in
        // research/podcast workflows) and must terminate even when a
        // child's child_session_key happens to point back to an already
        // visited session.
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let parent_session_key = SessionKey::new("api", "deep-research");

        // Level 1: parent spawns child A.
        let parent_supervisor = Arc::new(TaskSupervisor::new());
        let parent_ledger = data_dir.join("parent.jsonl");
        parent_supervisor
            .enable_persistence(&parent_ledger)
            .unwrap();
        let level1_id = parent_supervisor.register_with_lineage(
            "spawn",
            "call-l1",
            Some(&parent_session_key.to_string()),
            Some(parent_ledger.to_str().unwrap()),
        );
        let level1_child_key = parent_supervisor
            .get_task(&level1_id)
            .and_then(|t| t.child_session_key)
            .expect("level-1 child key");

        // Level 2: child A spawns child B.
        let mid_supervisor = Arc::new(TaskSupervisor::new());
        let mid_ledger = data_dir.join("mid.jsonl");
        mid_supervisor.enable_persistence(&mid_ledger).unwrap();
        let level2_id = mid_supervisor.register_with_lineage(
            "spawn",
            "call-l2",
            Some(&level1_child_key),
            Some(mid_ledger.to_str().unwrap()),
        );
        let level2_child_key = mid_supervisor
            .get_task(&level2_id)
            .and_then(|t| t.child_session_key)
            .expect("level-2 child key");

        // Level 3: leaf task running inside child B. We also register a
        // synthetic task whose child_session_key points back at the
        // already-visited parent — the visited guard must prevent a loop.
        let leaf_supervisor = Arc::new(TaskSupervisor::new());
        let leaf_ledger = data_dir.join("leaf.jsonl");
        leaf_supervisor.enable_persistence(&leaf_ledger).unwrap();
        let leaf_id = leaf_supervisor.register_with_lineage(
            "run_pipeline",
            "call-l3",
            Some(&level2_child_key),
            Some(leaf_ledger.to_str().unwrap()),
        );
        leaf_supervisor.mark_running(&leaf_id);

        let store = SessionTaskQueryStore::default();
        store.register(&parent_session_key, &parent_supervisor, &data_dir);
        store.register(
            &SessionKey(level1_child_key.clone()),
            &mid_supervisor,
            &data_dir,
        );
        store.register(
            &SessionKey(level2_child_key.clone()),
            &leaf_supervisor,
            &data_dir,
        );

        let payload = store.query_json(&parent_session_key.to_string());
        let tasks = payload.as_array().expect("array response");
        assert_eq!(
            tasks.len(),
            3,
            "depth-3 descendant traversal must surface every task exactly once"
        );

        let tool_names: std::collections::HashSet<&str> = tasks
            .iter()
            .filter_map(|t| t["tool_name"].as_str())
            .collect();
        assert!(tool_names.contains("spawn"));
        assert!(tool_names.contains("run_pipeline"));
    }

    // ── Mock providers for speculative overflow tests ────────────────────

    /// Mock LLM provider with configurable delay per call.
    /// Returns scripted responses in FIFO order.
    struct DelayedMockProvider {
        responses: std::sync::Mutex<Vec<(Duration, ChatResponse)>>,
        call_count: AtomicUsize,
        name: String,
    }

    impl DelayedMockProvider {
        fn new(name: &str, responses: Vec<(Duration, ChatResponse)>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                call_count: AtomicUsize::new(0),
                name: name.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for DelayedMockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            let (delay, response) = {
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    return Ok(ChatResponse {
                        content: Some("(no more scripted responses)".into()),
                        reasoning_content: None,
                        tool_calls: vec![],
                        stop_reason: StopReason::EndTurn,
                        usage: TokenUsage::default(),
                        provider_index: None,
                    });
                }
                responses.remove(0)
            };
            tokio::time::sleep(delay).await;
            Ok(response)
        }

        fn context_window(&self) -> u32 {
            128_000
        }

        fn model_id(&self) -> &str {
            &self.name
        }

        fn provider_name(&self) -> &str {
            &self.name
        }
    }

    /// Mock LLM provider that scripts a sequence of responses (like
    /// `DelayedMockProvider`) AND emits a single `StreamChunk` through the
    /// task-local `TASK_REPORTER` before returning each one. The stream
    /// chunk drives the overflow's `stream_forwarder` to call
    /// `channel.send_with_id`, so `stream_result.message_id` captures
    /// whatever that channel returns — exercising the API-channel path
    /// where `send_with_id` returns `Some("sse-{chat_id}")` and therefore
    /// triggers the `already_streamed` guard in `serve_overflow`.
    struct StreamingMockProvider {
        responses: std::sync::Mutex<Vec<(Duration, String, ChatResponse)>>,
        name: String,
    }

    impl StreamingMockProvider {
        fn new(name: &str, responses: Vec<(Duration, String, ChatResponse)>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                name: name.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for StreamingMockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            let (delay, stream_chunk, response) = {
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    return Ok(ChatResponse {
                        content: Some("(no more scripted responses)".into()),
                        reasoning_content: None,
                        tool_calls: vec![],
                        stop_reason: StopReason::EndTurn,
                        usage: TokenUsage::default(),
                        provider_index: None,
                    });
                }
                responses.remove(0)
            };
            // Push a `StreamChunk` into the task-local reporter so the
            // stream_forwarder sees it and calls `channel.send_with_id`.
            // `try_with` fails open when no reporter is scoped (e.g. when
            // called outside the overflow's TASK_REPORTER scope).
            if !stream_chunk.is_empty() {
                if let Ok(reporter) = octos_agent::TASK_REPORTER.try_with(|r| r.clone()) {
                    reporter.report(octos_agent::ProgressEvent::StreamChunk {
                        text: stream_chunk,
                        iteration: 1,
                    });
                    // Give the stream_forwarder a chance to flush the chunk
                    // through the channel (mimics real streaming latency).
                    tokio::task::yield_now().await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            tokio::time::sleep(delay).await;
            Ok(response)
        }

        fn context_window(&self) -> u32 {
            128_000
        }

        fn model_id(&self) -> &str {
            &self.name
        }

        fn provider_name(&self) -> &str {
            &self.name
        }
    }

    /// Mimics `ApiChannel::send_with_id`, which always returns
    /// `Some("sse-{chat_id}")` so the stream forwarder switches to
    /// `edit_message` for subsequent chunks. `edit_message` is a no-op
    /// here — equivalent to `pending[chat_id]` having been removed after
    /// the primary turn emitted its `_completion` marker. This setup
    /// reproduces FA-12 defect C exactly: the forwarder believes content
    /// was streamed (message_id is `Some`), but the web client's pending
    /// SSE channel never received the chunks.
    struct FakeSseChannel {
        name: String,
    }

    impl FakeSseChannel {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
            }
        }
    }

    #[async_trait]
    impl octos_bus::Channel for FakeSseChannel {
        fn name(&self) -> &str {
            &self.name
        }

        async fn start(
            &self,
            _inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
        ) -> eyre::Result<()> {
            Ok(())
        }

        async fn send(&self, _msg: &OutboundMessage) -> eyre::Result<()> {
            // No-op: the real ApiChannel writes to `pending[chat_id]` which
            // is removed when the primary turn emits `_completion`. We
            // simulate the "pending is already gone" state by dropping
            // everything silently.
            Ok(())
        }

        async fn send_with_id(&self, msg: &OutboundMessage) -> eyre::Result<Option<String>> {
            // Mirror ApiChannel::send_with_id exactly — always return
            // Some("sse-{chat_id}"), flipping `stream_result.message_id`
            // to Some and triggering the FA-12d defective branch.
            Ok(Some(format!("sse-{}", msg.chat_id)))
        }

        async fn edit_message(
            &self,
            _chat_id: &str,
            _message_id: &str,
            _new_content: &str,
        ) -> eyre::Result<()> {
            Ok(())
        }

        fn supports_edit(&self) -> bool {
            true
        }
    }

    struct ErrorMockProvider {
        name: String,
        error: String,
    }

    impl ErrorMockProvider {
        fn new(name: &str, error: &str) -> Self {
            Self {
                name: name.to_string(),
                error: error.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for ErrorMockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            Err(eyre::eyre!(self.error.clone()))
        }

        fn context_window(&self) -> u32 {
            128_000
        }

        fn model_id(&self) -> &str {
            &self.name
        }

        fn provider_name(&self) -> &str {
            &self.name
        }
    }

    fn make_response(text: &str) -> ChatResponse {
        ChatResponse {
            content: Some(text.to_string()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 10,
                ..Default::default()
            },
            provider_index: None,
        }
    }

    fn make_inbound(content: &str) -> ActorMessage {
        ActorMessage::Inbound {
            message: InboundMessage {
                channel: "cli".to_string(),
                chat_id: "test".to_string(),
                sender_id: "user".to_string(),
                content: content.to_string(),
                timestamp: chrono::Utc::now(),
                media: vec![],
                metadata: serde_json::json!({}),
                message_id: None,
            },
            image_media: vec![],
            attachment_media: vec![],
            attachment_prompt: None,
        }
    }

    fn make_attachment_inbound(summary: &str, attachment_path: &str) -> ActorMessage {
        ActorMessage::Inbound {
            message: InboundMessage {
                channel: "cli".to_string(),
                chat_id: "test".to_string(),
                sender_id: "user".to_string(),
                content: String::new(),
                timestamp: chrono::Utc::now(),
                media: vec![],
                metadata: serde_json::json!({}),
                message_id: None,
            },
            image_media: vec![],
            attachment_media: vec![attachment_path.to_string()],
            attachment_prompt: Some(summary.to_string()),
        }
    }

    /// Build a SessionActor with configurable queue mode and optional adaptive router.
    ///
    /// Generic setup used by queue mode, auto-escalation, and other tests.
    /// `adaptive_router` controls whether speculative overflow is available.
    /// `pre_seed_baseline`: if true, pre-seeds 5×500ms to establish responsiveness baseline.
    async fn setup_actor_with_mode(
        agent_provider: Arc<dyn LlmProvider>,
        queue_mode: QueueMode,
        adaptive_router: Option<Arc<AdaptiveRouter>>,
        pre_seed_baseline: bool,
        dir: &tempfile::TempDir,
    ) -> (
        mpsc::Sender<ActorMessage>,
        mpsc::Receiver<OutboundMessage>,
        JoinHandle<()>,
        Arc<Mutex<SessionManager>>,
    ) {
        let session_mgr = Arc::new(Mutex::new(
            SessionManager::open(&dir.path().join("sessions")).unwrap(),
        ));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let tools = octos_agent::ToolRegistry::with_builtins(dir.path());

        let agent = Agent::new(AgentId::new("test-mode"), agent_provider, tools, memory)
            .with_config(AgentConfig {
                save_episodes: false,
                max_iterations: 1,
                ..Default::default()
            });

        let (inbox_tx, inbox_rx) = mpsc::channel(32);
        let (out_tx, out_rx) = mpsc::channel(64);

        let mut responsiveness = ResponsivenessObserver::new();
        if pre_seed_baseline {
            for _ in 0..5 {
                responsiveness.record(Duration::from_millis(500));
            }
        }

        let actor = SessionActor {
            session_key: SessionKey::new("cli", "test"),
            channel: "cli".to_string(),
            chat_id: "test".to_string(),
            inbox: inbox_rx,
            agent: Arc::new(agent),
            hooks: None,
            hook_context: None,
            session_handle: Arc::new(Mutex::new(SessionHandle::open(
                dir.path(),
                &SessionKey::new("cli", "test"),
            ))),
            llm_for_compaction: Arc::new(DelayedMockProvider::new(
                "compaction",
                vec![(Duration::ZERO, make_response("compacted"))],
            )),
            out_tx,
            status_indicator: None,
            sender_user_id: None,
            user_status_config: UserStatusConfig::default(),
            data_dir: dir.path().to_path_buf(),
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(120),
            semaphore: Arc::new(Semaphore::new(10)),
            global_shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode,
            responsiveness,
            adaptive_router,
            memory_store: None,
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            user_workspace: dir.path().join("workspace"),
            cron_tool: None,
            persistent_retry_state: Arc::new(StdMutex::new(LoopRetryState::default())),
            retry_state_path: None,
            recovered_tasks: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        };

        let handle = tokio::spawn(actor.run());
        (inbox_tx, out_rx, handle, session_mgr)
    }

    async fn setup_actor_with_timeout(
        agent_provider: Arc<dyn LlmProvider>,
        session_timeout: Duration,
        dir: &tempfile::TempDir,
    ) -> (
        mpsc::Sender<ActorMessage>,
        mpsc::Receiver<OutboundMessage>,
        JoinHandle<()>,
        Arc<Mutex<SessionManager>>,
    ) {
        let session_mgr = Arc::new(Mutex::new(
            SessionManager::open(&dir.path().join("sessions")).unwrap(),
        ));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let tools = octos_agent::ToolRegistry::with_builtins(dir.path());

        let agent = Agent::new(AgentId::new("test-timeout"), agent_provider, tools, memory)
            .with_config(AgentConfig {
                save_episodes: false,
                max_iterations: 1,
                ..Default::default()
            });

        let (inbox_tx, inbox_rx) = mpsc::channel(32);
        let (out_tx, out_rx) = mpsc::channel(64);

        let actor = SessionActor {
            session_key: SessionKey::new("cli", "test"),
            channel: "cli".to_string(),
            chat_id: "test".to_string(),
            inbox: inbox_rx,
            agent: Arc::new(agent),
            hooks: None,
            hook_context: None,
            session_handle: Arc::new(Mutex::new(SessionHandle::open(
                dir.path(),
                &SessionKey::new("cli", "test"),
            ))),
            llm_for_compaction: Arc::new(DelayedMockProvider::new(
                "compaction",
                vec![(Duration::ZERO, make_response("compacted"))],
            )),
            out_tx,
            status_indicator: None,
            sender_user_id: None,
            user_status_config: UserStatusConfig::default(),
            data_dir: dir.path().to_path_buf(),
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout,
            semaphore: Arc::new(Semaphore::new(10)),
            global_shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode: QueueMode::Followup,
            responsiveness: ResponsivenessObserver::new(),
            adaptive_router: None,
            memory_store: None,
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            user_workspace: dir.path().join("workspace"),
            cron_tool: None,
            persistent_retry_state: Arc::new(StdMutex::new(LoopRetryState::default())),
            retry_state_path: None,
            recovered_tasks: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        };

        let handle = tokio::spawn(actor.run());
        (inbox_tx, out_rx, handle, session_mgr)
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_session_actor_emits_resume_and_turn_end_hooks() {
        let dir = tempfile::TempDir::new().unwrap();
        let hook_log = dir.path().join("session-hooks.jsonl");
        let hooks = Arc::new(HookExecutor::new(vec![
            capture_hook(HookEvent::OnResume, &hook_log),
            capture_hook(HookEvent::OnTurnEnd, &hook_log),
        ]));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let tools = octos_agent::ToolRegistry::with_builtins(dir.path());
        let agent = Agent::new(
            AgentId::new("test-hooks"),
            Arc::new(DelayedMockProvider::new(
                "hooks",
                vec![(Duration::ZERO, make_response("hook response"))],
            )),
            tools,
            memory,
        )
        .with_config(AgentConfig {
            save_episodes: false,
            max_iterations: 1,
            ..Default::default()
        });

        let (inbox_tx, inbox_rx) = mpsc::channel(32);
        let (out_tx, mut out_rx) = mpsc::channel(64);

        let actor = SessionActor {
            session_key: SessionKey::new("cli", "test"),
            channel: "cli".to_string(),
            chat_id: "test".to_string(),
            inbox: inbox_rx,
            agent: Arc::new(agent),
            hooks: Some(hooks),
            hook_context: Some(HookContext {
                session_id: Some("cli:test".to_string()),
                profile_id: Some("test-profile".to_string()),
            }),
            session_handle: Arc::new(Mutex::new(SessionHandle::open(
                dir.path(),
                &SessionKey::new("cli", "test"),
            ))),
            llm_for_compaction: Arc::new(DelayedMockProvider::new(
                "compaction",
                vec![(Duration::ZERO, make_response("compacted"))],
            )),
            out_tx,
            status_indicator: None,
            sender_user_id: None,
            user_status_config: UserStatusConfig::default(),
            data_dir: dir.path().to_path_buf(),
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(120),
            semaphore: Arc::new(Semaphore::new(10)),
            global_shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode: QueueMode::Followup,
            responsiveness: ResponsivenessObserver::new(),
            adaptive_router: None,
            memory_store: None,
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            user_workspace: dir.path().join("workspace"),
            cron_tool: None,
            persistent_retry_state: Arc::new(StdMutex::new(LoopRetryState::default())),
            retry_state_path: None,
            recovered_tasks: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        };

        let handle = tokio::spawn(actor.run());
        inbox_tx
            .send(make_inbound("hello   hook   turn"))
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), out_rx.recv())
            .await
            .unwrap();

        drop(inbox_tx);
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;

        let lines = std::fs::read_to_string(&hook_log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();

        assert!(
            lines
                .iter()
                .any(|line| line.contains("\"event\":\"on_resume\""))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("\"event\":\"on_turn_end\""))
        );
        assert!(
            lines
                .iter()
                .all(|line| line.contains("\"session_id\":\"cli:test\""))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("\"turn_summary\":\"hello hook turn\""))
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_forced_background_turn_emits_turn_end_hook() {
        let dir = tempfile::TempDir::new().unwrap();
        let hook_log = dir.path().join("forced-background-hooks.jsonl");
        let hooks = Arc::new(HookExecutor::new(vec![capture_hook(
            HookEvent::OnTurnEnd,
            &hook_log,
        )]));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let (inbox_tx, inbox_rx) = mpsc::channel::<ActorMessage>(32);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<InboundMessage>(32);
        let (out_tx, mut out_rx) = mpsc::channel(64);

        let mut tools = octos_agent::ToolRegistry::with_builtins(dir.path());
        tools.register(octos_agent::SpawnTool::new(
            Arc::new(DelayedMockProvider::new(
                "forced-background-worker",
                vec![(Duration::ZERO, make_response("background complete"))],
            )),
            Arc::clone(&memory),
            dir.path().to_path_buf(),
            spawn_tx,
        ));

        let agent = Agent::new(
            AgentId::new("test-forced-background-hooks"),
            Arc::new(DelayedMockProvider::new(
                "forced-background-primary",
                vec![(Duration::ZERO, make_response("foreground fallback"))],
            )),
            tools,
            memory,
        )
        .with_config(AgentConfig {
            save_episodes: false,
            max_iterations: 1,
            ..Default::default()
        });

        let actor = SessionActor {
            session_key: SessionKey::new("cli", "test"),
            channel: "cli".to_string(),
            chat_id: "test".to_string(),
            inbox: inbox_rx,
            agent: Arc::new(agent),
            hooks: Some(hooks),
            hook_context: Some(HookContext {
                session_id: Some("cli:test".to_string()),
                profile_id: Some("test-profile".to_string()),
            }),
            session_handle: Arc::new(Mutex::new(SessionHandle::open(
                dir.path(),
                &SessionKey::new("cli", "test"),
            ))),
            llm_for_compaction: Arc::new(DelayedMockProvider::new(
                "compaction",
                vec![(Duration::ZERO, make_response("compacted"))],
            )),
            out_tx,
            status_indicator: None,
            sender_user_id: None,
            user_status_config: UserStatusConfig::default(),
            data_dir: dir.path().to_path_buf(),
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(120),
            semaphore: Arc::new(Semaphore::new(10)),
            global_shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode: QueueMode::Followup,
            responsiveness: ResponsivenessObserver::new(),
            adaptive_router: None,
            memory_store: None,
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            user_workspace: dir.path().join("workspace"),
            cron_tool: None,
            persistent_retry_state: Arc::new(StdMutex::new(LoopRetryState::default())),
            retry_state_path: None,
            recovered_tasks: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        };

        let handle = tokio::spawn(actor.run());
        inbox_tx
            .send(make_inbound("请对这个主题做一次深度研究，并输出完整报告。"))
            .await
            .unwrap();

        let _ = tokio::time::timeout(Duration::from_secs(3), out_rx.recv())
            .await
            .unwrap()
            .unwrap();

        let started = tokio::time::Instant::now();
        loop {
            let lines = std::fs::read_to_string(&hook_log)
                .ok()
                .map(|contents| contents.lines().map(str::to_string).collect::<Vec<_>>())
                .unwrap_or_default();

            if lines
                .iter()
                .any(|line| line.contains("\"event\":\"on_turn_end\""))
            {
                assert!(lines.iter().any(|line| {
                    line.contains(
                        "\"turn_summary\":\"请对这个主题做一次深度研究，并输出完整报告。\"",
                    )
                }));
                break;
            }

            assert!(
                started.elapsed() < Duration::from_secs(5),
                "forced-background turn-end hook did not arrive in time"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        drop(inbox_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    async fn setup_actor_for_cron_regression(
        agent_provider: Arc<dyn LlmProvider>,
        dir: &tempfile::TempDir,
    ) -> (
        mpsc::Sender<ActorMessage>,
        mpsc::Receiver<OutboundMessage>,
        JoinHandle<()>,
        Arc<octos_bus::CronService>,
    ) {
        let _session_mgr = Arc::new(Mutex::new(
            SessionManager::open(&dir.path().join("sessions")).unwrap(),
        ));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let mut tools = octos_agent::ToolRegistry::with_builtins(dir.path());

        let (cron_tx, _cron_rx) = mpsc::channel(64);
        let cron_service = Arc::new(octos_bus::CronService::new(
            dir.path().join("cron.json"),
            cron_tx,
        ));
        let cron_tool = Arc::new(CronTool::with_context(cron_service.clone(), "cli", "test"));
        tools.register_arc(cron_tool.clone());

        let agent = Agent::new(
            AgentId::new("test-cron-regression"),
            agent_provider,
            tools,
            memory,
        )
        .with_config(AgentConfig {
            save_episodes: false,
            max_iterations: 6,
            ..Default::default()
        });

        let (inbox_tx, inbox_rx) = mpsc::channel(32);
        let (out_tx, out_rx) = mpsc::channel(64);

        let actor = SessionActor {
            session_key: SessionKey::new("cli", "test"),
            channel: "cli".to_string(),
            chat_id: "test".to_string(),
            inbox: inbox_rx,
            agent: Arc::new(agent),
            hooks: None,
            hook_context: None,
            session_handle: Arc::new(Mutex::new(SessionHandle::open(
                dir.path(),
                &SessionKey::new("cli", "test"),
            ))),
            llm_for_compaction: Arc::new(DelayedMockProvider::new(
                "compaction",
                vec![(Duration::ZERO, make_response("compacted"))],
            )),
            out_tx,
            status_indicator: None,
            sender_user_id: None,
            user_status_config: UserStatusConfig::default(),
            data_dir: dir.path().to_path_buf(),
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(120),
            semaphore: Arc::new(Semaphore::new(10)),
            global_shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode: QueueMode::Followup,
            responsiveness: ResponsivenessObserver::new(),
            adaptive_router: None,
            memory_store: None,
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            user_workspace: dir.path().join("workspace"),
            cron_tool: Some(cron_tool),
            persistent_retry_state: Arc::new(StdMutex::new(LoopRetryState::default())),
            retry_state_path: None,
            recovered_tasks: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        };

        let handle = tokio::spawn(actor.run());
        (inbox_tx, out_rx, handle, cron_service)
    }

    /// Build a minimal SessionActor with speculative mode + adaptive router.
    ///
    /// `agent_provider` is used by the Agent for primary calls.
    /// `router_providers` are used by the AdaptiveRouter for overflow calls.
    /// These MUST be separate instances (separate response queues).
    async fn setup_speculative_actor(
        agent_provider: Arc<dyn LlmProvider>,
        router_providers: Vec<Arc<dyn LlmProvider>>,
        dir: &tempfile::TempDir,
    ) -> (
        mpsc::Sender<ActorMessage>,
        mpsc::Receiver<OutboundMessage>,
        JoinHandle<()>,
        Arc<Mutex<SessionManager>>,
    ) {
        let session_mgr = Arc::new(Mutex::new(
            SessionManager::open(&dir.path().join("sessions")).unwrap(),
        ));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let tools = octos_agent::ToolRegistry::with_builtins(dir.path());

        let agent = Agent::new(AgentId::new("test-spec"), agent_provider, tools, memory)
            .with_config(AgentConfig {
                save_episodes: false,
                max_iterations: 1,
                ..Default::default()
            });

        // AdaptiveRouter with separate providers for overflow (serve_overflow only)
        let router = Arc::new(
            AdaptiveRouter::new(router_providers, &[], AdaptiveConfig::default())
                .with_adaptive_config(AdaptiveMode::Hedge, false),
        );

        let (inbox_tx, inbox_rx) = mpsc::channel(32);
        let (out_tx, out_rx) = mpsc::channel(64);

        // Pre-seed responsiveness baseline so patience = 10s (not 30s default)
        let mut responsiveness = ResponsivenessObserver::new();
        for _ in 0..5 {
            responsiveness.record(Duration::from_millis(500));
        }
        // baseline = 500ms → patience = max(1000ms, 10s) = 10s
        // But we want lower patience for fast tests. We'll use 2s responses
        // to establish baseline=2s → patience=max(4s, 10s)=10s.
        // For the test, the slow call takes 15s, so 15s > 10s triggers overflow.

        let actor = SessionActor {
            session_key: SessionKey::new("cli", "test"),
            channel: "cli".to_string(),
            chat_id: "test".to_string(),
            inbox: inbox_rx,
            agent: Arc::new(agent),
            hooks: None,
            hook_context: None,
            session_handle: Arc::new(Mutex::new(SessionHandle::open(
                dir.path(),
                &SessionKey::new("cli", "test"),
            ))),
            llm_for_compaction: Arc::new(DelayedMockProvider::new(
                "compaction",
                vec![(Duration::ZERO, make_response("compacted"))],
            )),
            out_tx,
            status_indicator: None,
            sender_user_id: None,
            user_status_config: UserStatusConfig::default(),
            data_dir: dir.path().to_path_buf(),
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(120),
            semaphore: Arc::new(Semaphore::new(10)),
            global_shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode: QueueMode::Speculative,
            responsiveness,
            adaptive_router: Some(router),
            memory_store: None,
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            user_workspace: dir.path().join("workspace"),
            cron_tool: None,
            persistent_retry_state: Arc::new(StdMutex::new(LoopRetryState::default())),
            retry_state_path: None,
            recovered_tasks: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        };

        let handle = tokio::spawn(actor.run());
        (inbox_tx, out_rx, handle, session_mgr)
    }

    /// Variant of `setup_speculative_actor` that wires a real
    /// `StatusComposer` backed by a caller-supplied `Channel`. Used by the
    /// FA-12d regression test to route the overflow stream through a
    /// channel whose `send_with_id` returns `Some("sse-{chat_id}")`, so
    /// `stream_result.message_id.is_some()` evaluates to true.
    async fn setup_speculative_actor_with_indicator(
        agent_provider: Arc<dyn LlmProvider>,
        router_providers: Vec<Arc<dyn LlmProvider>>,
        status_channel: Arc<dyn octos_bus::Channel>,
        reply_channel: &str,
        dir: &tempfile::TempDir,
    ) -> (
        mpsc::Sender<ActorMessage>,
        mpsc::Receiver<OutboundMessage>,
        JoinHandle<()>,
        Arc<Mutex<SessionManager>>,
    ) {
        let session_mgr = Arc::new(Mutex::new(
            SessionManager::open(&dir.path().join("sessions")).unwrap(),
        ));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let tools = octos_agent::ToolRegistry::with_builtins(dir.path());

        let agent = Agent::new(AgentId::new("test-spec-api"), agent_provider, tools, memory)
            .with_config(AgentConfig {
                save_episodes: false,
                max_iterations: 1,
                ..Default::default()
            });

        let router = Arc::new(
            AdaptiveRouter::new(router_providers, &[], AdaptiveConfig::default())
                .with_adaptive_config(AdaptiveMode::Hedge, false),
        );

        let (inbox_tx, inbox_rx) = mpsc::channel(32);
        let (out_tx, out_rx) = mpsc::channel(64);

        let mut responsiveness = ResponsivenessObserver::new();
        for _ in 0..5 {
            responsiveness.record(Duration::from_millis(500));
        }

        // StatusComposer with our fake SSE channel — its `.channel()` is used
        // by `run_stream_forwarder` to send/edit streaming chunks.
        let status_indicator =
            Arc::new(StatusComposer::new(status_channel, vec!["Thinking".into()]));

        let session_key = SessionKey::new(reply_channel, "test-api-chat");
        let actor = SessionActor {
            session_key: session_key.clone(),
            channel: reply_channel.to_string(),
            chat_id: "test-api-chat".to_string(),
            inbox: inbox_rx,
            agent: Arc::new(agent),
            hooks: None,
            hook_context: None,
            session_handle: Arc::new(Mutex::new(SessionHandle::open(dir.path(), &session_key))),
            llm_for_compaction: Arc::new(DelayedMockProvider::new(
                "compaction",
                vec![(Duration::ZERO, make_response("compacted"))],
            )),
            out_tx,
            status_indicator: Some(status_indicator),
            sender_user_id: None,
            user_status_config: UserStatusConfig::default(),
            data_dir: std::path::PathBuf::from("/tmp"),
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(120),
            semaphore: Arc::new(Semaphore::new(10)),
            global_shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode: QueueMode::Speculative,
            responsiveness,
            adaptive_router: Some(router),
            memory_store: None,
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            user_workspace: dir.path().join("workspace"),
            cron_tool: None,
            persistent_retry_state: Arc::new(StdMutex::new(LoopRetryState::default())),
            retry_state_path: None,
            recovered_tasks: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        };

        let handle = tokio::spawn(actor.run());
        (inbox_tx, out_rx, handle, session_mgr)
    }

    /// Inbound helper that matches the fake SSE channel's chat_id.
    fn make_inbound_api(content: &str, reply_channel: &str) -> ActorMessage {
        ActorMessage::Inbound {
            message: InboundMessage {
                channel: reply_channel.to_string(),
                chat_id: "test-api-chat".to_string(),
                sender_id: "user".to_string(),
                content: content.to_string(),
                timestamp: chrono::Utc::now(),
                media: vec![],
                metadata: serde_json::json!({}),
                message_id: Some("client-msg-bravo".to_string()),
            },
            image_media: vec![],
            attachment_media: vec![],
            attachment_prompt: None,
        }
    }

    /// Core speculative overflow test:
    /// - Send a message that triggers a slow (3s) agent call
    /// - After 1s, send an overflow message
    /// - The overflow should be served via serve_overflow while the slow call continues
    /// - Both responses should arrive
    #[tokio::test]
    async fn test_cron_timezone_reset_regression_chinese_transcript() {
        let dir = tempfile::TempDir::new().unwrap();
        let provider = Arc::new(DelayedMockProvider::new(
            "cron-regression",
            vec![
                (
                    Duration::ZERO,
                    make_response("好的，我记住了，你的时区是 PDT。"),
                ),
                (
                    Duration::ZERO,
                    make_response(
                        "<invoke name=\"cron\">{\"action\":\"add\",\"message\":\"10分钟后提醒喝水\",\"after_seconds\":600,\"name\":\"drink-water\",\"timezone\":\"America/Los_Angeles\"}</invoke>",
                    ),
                ),
                (
                    Duration::ZERO,
                    make_response("已设置好，10分钟后提醒你喝水。"),
                ),
                (
                    Duration::ZERO,
                    make_response("<invoke name=\"cron\">{\"action\":\"list\"}</invoke>"),
                ),
                (Duration::ZERO, make_response("当前已有提醒任务。")),
                (
                    Duration::ZERO,
                    make_response(
                        "<invoke name=\"cron\">{\"action\":\"add\",\"message\":\"10分钟后提醒站起来活动\",\"after_seconds\":600,\"name\":\"stand-up\",\"timezone\":\"America/Los_Angeles\"}</invoke>",
                    ),
                ),
                (
                    Duration::ZERO,
                    make_response("重置后也已设置，10分钟后提醒你站起来活动。"),
                ),
            ],
        ));

        let (tx, mut rx, handle, cron_service) =
            setup_actor_for_cron_regression(provider.clone(), &dir).await;

        tx.send(make_inbound("把我的时区记成PDT")).await.unwrap();
        let r1 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!r1.content.contains("<invoke"));
        assert!(r1.content.contains("PDT"));

        let before_first_add = chrono::Utc::now().timestamp_millis();
        tx.send(make_inbound("10分钟后提醒我喝水")).await.unwrap();
        let r2 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let after_first_add = chrono::Utc::now().timestamp_millis();
        assert!(!r2.content.contains("<invoke"));
        assert!(r2.content.contains("10分钟"));

        let jobs = cron_service.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "drink-water");
        assert_eq!(jobs[0].timezone.as_deref(), Some("America/Los_Angeles"));
        let first_at_ms = match jobs[0].schedule {
            octos_bus::CronSchedule::At { at_ms } => at_ms,
            _ => panic!("expected one-time reminder"),
        };
        assert!(
            first_at_ms >= before_first_add + 600_000 && first_at_ms <= after_first_add + 603_000,
            "first at_ms out of expected range: {}",
            first_at_ms
        );

        tx.send(make_inbound("列出提醒")).await.unwrap();
        let r3 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!r3.content.contains("<invoke"));
        assert!(r3.content.contains("提醒"));

        tx.send(make_inbound("/reset")).await.unwrap();
        let reset_reply = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(reset_reply.content.contains("history cleared"));

        let before_second_add = chrono::Utc::now().timestamp_millis();
        tx.send(make_inbound("重置后，再过10分钟提醒我站起来活动"))
            .await
            .unwrap();
        let r4 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let after_second_add = chrono::Utc::now().timestamp_millis();
        assert!(!r4.content.contains("<invoke"));
        assert!(r4.content.contains("重置后"));

        let jobs = cron_service.list_jobs();
        assert_eq!(jobs.len(), 2);
        let second = jobs.iter().find(|j| j.name == "stand-up").unwrap();
        let second_at_ms = match second.schedule {
            octos_bus::CronSchedule::At { at_ms } => at_ms,
            _ => panic!("expected one-time reminder"),
        };
        assert!(second_at_ms > first_at_ms);
        assert!(
            second_at_ms >= before_second_add + 600_000
                && second_at_ms <= after_second_add + 603_000,
            "second at_ms out of expected range: {}",
            second_at_ms
        );

        assert_eq!(provider.call_count.load(Ordering::SeqCst), 7);

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn test_speculative_overflow_concurrent() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent provider: 5 fast warmups + 1 slow (12s) primary call
        // + 1 fast overflow response (serve_overflow now uses the agent)
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(200), make_response("warmup1")),
                (Duration::from_millis(200), make_response("warmup2")),
                (Duration::from_millis(200), make_response("warmup3")),
                (Duration::from_millis(200), make_response("warmup4")),
                (Duration::from_millis(200), make_response("warmup5")),
                // Slow call that triggers overflow (12s > 10s patience)
                (
                    Duration::from_secs(12),
                    make_response("slow primary answer"),
                ),
                // Overflow agent task (runs concurrently with slow primary)
                (
                    Duration::from_millis(500),
                    make_response("overflow answer: 1961"),
                ),
                (Duration::from_millis(200), make_response("post-overflow")),
            ],
        ));

        // Router providers (separate instances, used ONLY by serve_overflow)
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-a",
            vec![
                (
                    Duration::from_millis(500),
                    make_response("router-a overflow"),
                ),
                (Duration::from_millis(500), make_response("router-a extra")),
            ],
        ));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-b",
            vec![
                (
                    Duration::from_millis(100),
                    make_response("overflow answer: 1961"),
                ),
                (Duration::from_millis(100), make_response("router-b extra")),
            ],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

        // ── Phase 1: Warm-up (5 fast messages to establish baseline) ──
        for i in 0..5 {
            tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
            // Wait for response
            let resp = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("warmup response timeout")
                .expect("channel closed");
            assert!(!resp.content.is_empty(), "warmup {i} got empty response");
        }

        // ── Phase 2: Send slow request, then overflow ──
        tx.send(make_inbound("Do a complex multi-step analysis"))
            .await
            .unwrap();

        // Wait 11s for patience (10s) to be exceeded, then send overflow
        tokio::time::sleep(Duration::from_secs(11)).await;

        tx.send(make_inbound("What is 37 * 53?")).await.unwrap();

        // ── Phase 3: Collect all responses ──
        // We expect 2 user-facing responses: overflow answer + slow primary
        // answer (in some order). Skip metadata-only outbounds (the
        // user-message session_result emission added by #616 fix carries
        // routing metadata in `_session_result` but no body).
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while responses.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => {
                    let is_user_session_result = msg
                        .metadata
                        .get("_session_result")
                        .and_then(|r| r.get("role"))
                        .and_then(|v| v.as_str())
                        == Some("user");
                    if !is_user_session_result {
                        responses.push(msg.content);
                    }
                }
                Ok(None) => break,
                Err(_) => break, // timeout
            }
        }

        assert!(
            responses.len() >= 2,
            "expected at least 2 responses (overflow + primary), got {}: {:?}",
            responses.len(),
            responses
        );

        // One should be the overflow answer, one the primary (with ⬆️ marker)
        let has_overflow = responses
            .iter()
            .any(|r| r.contains("1961") || r.contains("overflow"));
        let has_primary = responses
            .iter()
            .any(|r| r.contains("slow primary") || r.contains("primary"));

        assert!(
            has_overflow,
            "overflow response not found in: {:?}",
            responses
        );
        assert!(
            has_primary,
            "primary response not found in: {:?}",
            responses
        );

        // ── Phase 4: Verify history is sorted by timestamp ──
        {
            // Reload from disk (actor writes via its own SessionHandle to per-user dir)
            let handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
            let session = handle.session();
            let messages = &session.messages;
            assert!(
                messages.len() >= 4,
                "expected at least 4 messages in history (warmups + primary + overflow), got {}",
                messages.len()
            );

            // Verify timestamps are sorted
            for window in messages.windows(2) {
                assert!(
                    window[0].timestamp <= window[1].timestamp,
                    "history not sorted: {:?} > {:?} (contents: '{}' vs '{}')",
                    window[0].timestamp,
                    window[1].timestamp,
                    &window[0].content[..window[0].content.len().min(50)],
                    &window[1].content[..window[1].content.len().min(50)],
                );
            }
        }

        // Clean shutdown
        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// FA-11 defect B regression: the overflow assistant reply MUST carry
    /// `_session_result` metadata so `ApiChannel::send` can route it via
    /// `broadcast_session_event → watchers`. Without this metadata the reply
    /// routes only through `pending[session_id]`, which was removed when
    /// the primary turn emitted its `_completion` marker — so the overflow
    /// reply was silently dropped.
    #[tokio::test]
    async fn should_emit_session_result_metadata_for_overflow_reply() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 5 fast warmups + slow (12s) primary + fast overflow response.
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(200), make_response("warmup1")),
                (Duration::from_millis(200), make_response("warmup2")),
                (Duration::from_millis(200), make_response("warmup3")),
                (Duration::from_millis(200), make_response("warmup4")),
                (Duration::from_millis(200), make_response("warmup5")),
                (
                    Duration::from_secs(12),
                    make_response("slow primary answer"),
                ),
                (
                    Duration::from_millis(400),
                    make_response("overflow FA12 result payload"),
                ),
                (Duration::from_millis(200), make_response("post-overflow")),
            ],
        ));
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-a",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-b",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

        // Warmup to establish responsiveness baseline.
        for i in 0..5 {
            tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        }

        // Slow primary prompt.
        tx.send(make_inbound("please run a big analysis"))
            .await
            .unwrap();

        // Wait past patience (10s) so the second prompt is served as overflow.
        tokio::time::sleep(Duration::from_secs(11)).await;
        tx.send(make_inbound("please answer FA-12 probe"))
            .await
            .unwrap();

        // Collect OutboundMessage records until we've seen both non-empty
        // replies (overflow + slow primary).
        let mut outbound_replies: Vec<OutboundMessage> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while outbound_replies.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => {
                    if !msg.content.trim().is_empty() {
                        outbound_replies.push(msg);
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }

        assert!(
            outbound_replies.len() >= 2,
            "expected at least 2 replies (overflow + primary), got {}: {:?}",
            outbound_replies.len(),
            outbound_replies
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
        );

        let overflow = outbound_replies
            .iter()
            .find(|msg| msg.content.contains("FA12") || msg.content.contains("overflow"))
            .expect("overflow reply not found");
        let session_result = overflow.metadata.get("_session_result").unwrap_or_else(|| {
            panic!(
                "overflow outbound must carry `_session_result` metadata — \
                 got metadata = {}",
                overflow.metadata
            )
        });
        assert_eq!(
            session_result.get("role").and_then(|v| v.as_str()),
            Some("assistant"),
            "session_result role must be 'assistant'"
        );
        assert!(
            session_result.get("seq").and_then(|v| v.as_u64()).is_some(),
            "session_result must include committed seq, got {}",
            session_result
        );
        assert!(
            session_result
                .get("content")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.contains("FA12") || s.contains("overflow")),
            "session_result.content must match reply content, got {}",
            session_result
        );
        assert!(
            session_result.get("timestamp").is_some(),
            "session_result must include rfc3339 timestamp, got {}",
            session_result
        );
        assert!(
            overflow
                .metadata
                .get("_history_persisted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            "overflow outbound must flag history as persisted"
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// #616 regression: the overflow USER message must emit `_session_result`
    /// metadata so the web client can bind streaming response tokens to the
    /// overflow user-message bubble. Without this signal, when a fast follow-up
    /// arrives mid-primary-turn, the web client receives streaming tokens with
    /// no way to route them to the second user's bubble — the response renders
    /// nowhere (or worse, overwrites the primary's bubble).
    ///
    /// 14ac3f3a removed this emission on the assumption that timestamp-primary
    /// sort handles ordering. True for ordering, false for routing — both
    /// roles are needed and they're complementary, not exclusive.
    #[tokio::test]
    async fn should_emit_session_result_for_overflow_user_message() {
        let dir = tempfile::TempDir::new().unwrap();

        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(200), make_response("warmup1")),
                (Duration::from_millis(200), make_response("warmup2")),
                (Duration::from_millis(200), make_response("warmup3")),
                (Duration::from_millis(200), make_response("warmup4")),
                (Duration::from_millis(200), make_response("warmup5")),
                (Duration::from_secs(12), make_response("slow primary")),
                (Duration::from_millis(400), make_response("overflow body")),
                (Duration::from_millis(200), make_response("post-overflow")),
            ],
        ));
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-a",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-b",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

        // Warmup so responsiveness baseline is established.
        for i in 0..5 {
            tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        }

        // Slow primary.
        tx.send(make_inbound("please run a big analysis"))
            .await
            .unwrap();

        // Sleep past patience so the second prompt is served as overflow.
        tokio::time::sleep(Duration::from_secs(11)).await;

        // Fast follow-up with a known client_message_id so we can assert it
        // round-trips on the session_result event. Construct the Inbound
        // variant inline so we can set message_id (= client_message_id on the
        // wire from api_channel.rs:1222 — see #616 audit).
        let overflow_inbound = ActorMessage::Inbound {
            message: InboundMessage {
                channel: "cli".to_string(),
                chat_id: "test".to_string(),
                sender_id: "user".to_string(),
                content: "the overflow user question".to_string(),
                timestamp: chrono::Utc::now(),
                media: vec![],
                // Both fields carry client_message_id in production: api_channel
                // sets metadata["client_message_id"] (which `inbound_client_message_id`
                // reads) and message_id (which becomes overflow_reply_to). Mirror
                // both so we exercise the same path.
                metadata: serde_json::json!({
                    "client_message_id": "client-msg-overflow-test",
                }),
                message_id: Some("client-msg-overflow-test".to_string()),
            },
            image_media: vec![],
            attachment_media: vec![],
            attachment_prompt: None,
        };
        tx.send(overflow_inbound).await.unwrap();

        // Collect outbound until we see a user-role session_result (which is
        // the overflow user-message emission we're asserting on).
        let mut user_session_result: Option<serde_json::Value> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while user_session_result.is_none() {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => {
                    if let Some(result) = msg.metadata.get("_session_result") {
                        if result.get("role").and_then(|v| v.as_str()) == Some("user") {
                            user_session_result = Some(result.clone());
                        }
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }

        let result = user_session_result.unwrap_or_else(|| {
            panic!("overflow user message must emit _session_result with role=user")
        });

        assert_eq!(
            result.get("role").and_then(|v| v.as_str()),
            Some("user"),
            "role must be user"
        );
        assert!(
            result.get("seq").and_then(|v| v.as_u64()).is_some(),
            "session_result must include committed seq for the user message"
        );
        assert_eq!(
            result.get("content").and_then(|v| v.as_str()),
            Some("the overflow user question"),
            "content must mirror the overflow user message"
        );
        assert_eq!(
            result.get("client_message_id").and_then(|v| v.as_str()),
            Some("client-msg-overflow-test"),
            "client_message_id must round-trip from inbound — this is what the \
             web client uses to bind subsequent streaming tokens to the \
             overflow user bubble"
        );
        assert!(
            result.get("timestamp").is_some(),
            "session_result must include rfc3339 timestamp"
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// M8.10 PR #2 regression: every outbound message that fans out into
    /// SSE events on the API channel MUST carry `thread_id` metadata
    /// (= the user message's client_message_id) so the wire-side `done`,
    /// `replace`, `file`, etc. payloads can be tagged. Drives the same
    /// 2-POST rapid-succession pattern as
    /// `should_emit_session_result_for_overflow_user_message` and asserts
    /// that BOTH threads' outbound messages have thread_id populated and
    /// match the expected user cmid for that message's logical thread.
    ///
    /// The whole point of M8.10 is that overflow stops being a special
    /// case — same code path, same events, just a different thread_id.
    #[tokio::test]
    async fn should_emit_thread_id_on_every_event_for_speculative_overflow_pair() {
        let dir = tempfile::TempDir::new().unwrap();

        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(200), make_response("warmup1")),
                (Duration::from_millis(200), make_response("warmup2")),
                (Duration::from_millis(200), make_response("warmup3")),
                (Duration::from_millis(200), make_response("warmup4")),
                (Duration::from_millis(200), make_response("warmup5")),
                (
                    Duration::from_secs(12),
                    make_response("primary thread reply"),
                ),
                (
                    Duration::from_millis(400),
                    make_response("overflow thread reply"),
                ),
                (Duration::from_millis(200), make_response("post-overflow")),
            ],
        ));
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-a",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-b",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

        // Warmup so responsiveness baseline is established.
        for i in 0..5 {
            tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        }

        // Primary (slow) prompt with its own cmid.
        let primary_cmid = "cmid-primary-thread-A";
        let primary_inbound = ActorMessage::Inbound {
            message: InboundMessage {
                channel: "cli".to_string(),
                chat_id: "test".to_string(),
                sender_id: "user".to_string(),
                content: "primary slow prompt".to_string(),
                timestamp: chrono::Utc::now(),
                media: vec![],
                metadata: serde_json::json!({
                    "client_message_id": primary_cmid,
                }),
                message_id: Some(primary_cmid.to_string()),
            },
            image_media: vec![],
            attachment_media: vec![],
            attachment_prompt: None,
        };
        tx.send(primary_inbound).await.unwrap();

        // Sleep past patience so the second prompt is served as overflow.
        tokio::time::sleep(Duration::from_secs(11)).await;

        // Overflow follow-up with a DIFFERENT cmid.
        let overflow_cmid = "cmid-overflow-thread-B";
        let overflow_inbound = ActorMessage::Inbound {
            message: InboundMessage {
                channel: "cli".to_string(),
                chat_id: "test".to_string(),
                sender_id: "user".to_string(),
                content: "overflow follow-up".to_string(),
                timestamp: chrono::Utc::now(),
                media: vec![],
                metadata: serde_json::json!({
                    "client_message_id": overflow_cmid,
                }),
                message_id: Some(overflow_cmid.to_string()),
            },
            image_media: vec![],
            attachment_media: vec![],
            attachment_prompt: None,
        };
        tx.send(overflow_inbound).await.unwrap();

        // Collect outbound messages until both replies have arrived.
        let mut outbounds: Vec<OutboundMessage> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => {
                    outbounds.push(msg);
                    let primary_reply = outbounds
                        .iter()
                        .any(|m| m.content.contains("primary thread reply"));
                    let overflow_reply = outbounds
                        .iter()
                        .any(|m| m.content.contains("overflow thread reply"));
                    if primary_reply && overflow_reply {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }

        // Tag each outbound with the cmid of the thread it belongs to,
        // identified by content fingerprint. Filter out warmup leftovers
        // and unrelated metadata-only messages.
        let mut primary_outbounds: Vec<&OutboundMessage> = Vec::new();
        let mut overflow_outbounds: Vec<&OutboundMessage> = Vec::new();
        for msg in &outbounds {
            // Match by user cmid first (covers user-message session_result
            // emissions which echo the cmid).
            let session_result_cmid = msg
                .metadata
                .get("_session_result")
                .and_then(|sr| sr.get("client_message_id"))
                .and_then(|v| v.as_str());
            if session_result_cmid == Some(primary_cmid)
                || msg.content.contains("primary thread reply")
            {
                primary_outbounds.push(msg);
                continue;
            }
            if session_result_cmid == Some(overflow_cmid)
                || msg.content.contains("overflow thread reply")
            {
                overflow_outbounds.push(msg);
                continue;
            }
        }

        assert!(
            !primary_outbounds.is_empty(),
            "expected at least one outbound for the primary thread, got outbounds = {:?}",
            outbounds
                .iter()
                .map(|m| (m.content.as_str(), m.metadata.clone()))
                .collect::<Vec<_>>()
        );
        assert!(
            !overflow_outbounds.is_empty(),
            "expected at least one outbound for the overflow thread, got outbounds = {:?}",
            outbounds
                .iter()
                .map(|m| (m.content.as_str(), m.metadata.clone()))
                .collect::<Vec<_>>()
        );

        // Helper: assert that an outbound's metadata.thread_id matches
        // the expected cmid for its logical thread. Skip outbounds that
        // are pure-content (no metadata fan-out), since those don't go
        // through the API channel's SSE wrapping.
        fn assert_thread_id(msg: &OutboundMessage, expected_cmid: &str) {
            let actual = msg.metadata.get("thread_id").and_then(|v| v.as_str());
            assert_eq!(
                actual,
                Some(expected_cmid),
                "outbound for thread `{expected_cmid}` is missing thread_id metadata; \
                 content = {:?}, metadata = {}",
                msg.content,
                msg.metadata,
            );
        }

        // Every primary-thread outbound that carries fanout metadata must
        // bear the primary cmid. Overflow likewise.
        for msg in &primary_outbounds {
            // Filter to outbounds that produce SSE events: assistant reply
            // (non-empty content) OR completion marker OR session_result
            // user-message emission.
            let is_sse_producing = !msg.content.trim().is_empty()
                || msg.metadata.get("_completion").is_some()
                || msg.metadata.get("_session_result").is_some();
            if is_sse_producing {
                assert_thread_id(msg, primary_cmid);
            }
        }
        for msg in &overflow_outbounds {
            let is_sse_producing = !msg.content.trim().is_empty()
                || msg.metadata.get("_completion").is_some()
                || msg.metadata.get("_session_result").is_some();
            if is_sse_producing {
                assert_thread_id(msg, overflow_cmid);
            }
        }

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Regression: when the speculative path serves an overflow during a slow
    /// primary, the primary turn's final assistant reply must NOT be wrapped
    /// in the legacy "⬆️ Earlier task completed:" prefix. Users misread the
    /// prefix as a stray prior reply when it actually meant "I also processed
    /// your follow-up below in parallel" — so the prefix is gone and tool
    /// chips / message timeline carry the same meaning unambiguously.
    #[tokio::test]
    async fn should_drop_earlier_task_completed_prefix_when_overflow_served() {
        let dir = tempfile::TempDir::new().unwrap();

        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(200), make_response("warmup1")),
                (Duration::from_millis(200), make_response("warmup2")),
                (Duration::from_millis(200), make_response("warmup3")),
                (Duration::from_millis(200), make_response("warmup4")),
                (Duration::from_millis(200), make_response("warmup5")),
                (
                    Duration::from_secs(12),
                    make_response("PRIMARY_REPLY_BODY_marker"),
                ),
                (
                    Duration::from_millis(400),
                    make_response("OVERFLOW_REPLY_BODY_marker"),
                ),
                (Duration::from_millis(200), make_response("post-overflow")),
            ],
        ));
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-a",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-b",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

        for i in 0..5 {
            tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        }

        tx.send(make_inbound("please run a big analysis"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(11)).await;
        tx.send(make_inbound("name follow-up")).await.unwrap();

        let mut replies: Vec<OutboundMessage> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while replies.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => {
                    if !msg.content.trim().is_empty() {
                        replies.push(msg);
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }

        // Confirm both replies arrived (sanity for the overflow scenario).
        assert!(
            replies.len() >= 2,
            "expected primary + overflow replies, got {}",
            replies.len()
        );

        // No reply should carry the legacy prefix any longer.
        for reply in &replies {
            assert!(
                !reply.content.contains("Earlier task completed"),
                "legacy '⬆️ Earlier task completed:' prefix must be dropped, \
                 but reply contained it: {}",
                reply.content
            );
        }

        // The primary reply must surface its body unchanged (no leading
        // boilerplate that the user has to read past).
        let primary = replies
            .iter()
            .find(|m| m.content.contains("PRIMARY_REPLY_BODY_marker"))
            .expect("primary reply not found in collected outbound messages");
        assert!(
            primary.content.starts_with("PRIMARY_REPLY_BODY_marker"),
            "primary reply must start with its own body (no prefix), got: {}",
            primary.content
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// FA-12d defect-C regression: when the overflow runs against an
    /// `ApiChannel`-like transport (whose `send_with_id` always returns
    /// `Some("sse-{chat_id}")`) and the stream forwarder has flushed at
    /// least one chunk, the old code set `already_streamed = true` and
    /// silently skipped the `_session_result` emission — leaving the web
    /// client's Q2 bubble blank. The durable watchers fanout only fires
    /// when `ApiChannel::send` sees `_session_result` metadata, so the
    /// emission MUST happen regardless of `stream_result.message_id`.
    ///
    /// Guards the fix that decouples the durable metadata emission from
    /// the user-facing content rendering: the `_session_result` fanout
    /// always runs; only the outbound content body is suppressed when the
    /// channel already streamed the reply inline.
    #[tokio::test]
    async fn should_emit_session_result_metadata_for_api_channel_overflow_when_already_streamed() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent LLM: 5 fast warmups, slow (12s) primary, and a streaming
        // overflow response. `StreamingMockProvider` pushes a `StreamChunk`
        // into `TASK_REPORTER` before each response — on the overflow call
        // that flows through `run_stream_forwarder` →
        // `FakeSseChannel::send_with_id` → sets `message_id = Some(...)`,
        // so `stream_result.message_id.is_some() == true` and the
        // `already_streamed` branch is entered.
        //
        // `serve_overflow` invokes `agent.process_message_tracked` (NOT
        // the adaptive router) for the overflow, so the agent's provider
        // must emit the streaming chunk on the overflow call.
        let agent_llm = Arc::new(StreamingMockProvider::new(
            "agent-api",
            vec![
                (
                    Duration::from_millis(200),
                    String::new(),
                    make_response("warmup1"),
                ),
                (
                    Duration::from_millis(200),
                    String::new(),
                    make_response("warmup2"),
                ),
                (
                    Duration::from_millis(200),
                    String::new(),
                    make_response("warmup3"),
                ),
                (
                    Duration::from_millis(200),
                    String::new(),
                    make_response("warmup4"),
                ),
                (
                    Duration::from_millis(200),
                    String::new(),
                    make_response("warmup5"),
                ),
                (
                    Duration::from_secs(12),
                    String::new(),
                    make_response("slow primary answer"),
                ),
                (
                    Duration::from_millis(300),
                    "streaming chunk".into(),
                    make_response("FA12d overflow BRAVO answer"),
                ),
            ],
        ));

        // AdaptiveRouter providers are unused by the overflow path
        // (`serve_overflow` calls the agent directly) but the actor
        // requires the router to be wired so speculative mode is enabled.
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-a",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-b",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));

        let status_channel: Arc<dyn octos_bus::Channel> = Arc::new(FakeSseChannel::new("api"));
        let (tx, mut rx, handle, _session_mgr) = setup_speculative_actor_with_indicator(
            agent_llm,
            vec![router_a, router_b],
            status_channel,
            "api",
            &dir,
        )
        .await;

        // Warmup loop to establish responsiveness baseline; drain replies
        // from the channel as they come in (don't filter on content since
        // the new fix may emit empty-content OutboundMessages alongside
        // session_result metadata).
        for i in 0..5 {
            tx.send(make_inbound_api(&format!("warmup {i}"), "api"))
                .await
                .unwrap();
            // Drain until we see a _completion marker or timeout.
            let warmup_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while let Ok(Some(msg)) = tokio::time::timeout_at(warmup_deadline, rx.recv()).await {
                if msg.metadata.get("_completion").is_some() {
                    break;
                }
            }
        }

        // Slow primary prompt.
        tx.send(make_inbound_api("please run a big analysis", "api"))
            .await
            .unwrap();

        // Wait past patience (10s) so the next prompt is served as overflow.
        tokio::time::sleep(Duration::from_secs(11)).await;
        tx.send(make_inbound_api("please answer FA-12d probe", "api"))
            .await
            .unwrap();

        // Collect every OutboundMessage until we find one carrying the
        // overflow's `_session_result` metadata, or we timeout.
        let mut outbound_log: Vec<OutboundMessage> = Vec::new();
        let mut overflow_emission: Option<OutboundMessage> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => {
                    let carries_overflow_session_result = msg
                        .metadata
                        .get("_session_result")
                        .and_then(|sr| sr.get("content"))
                        .and_then(|c| c.as_str())
                        .is_some_and(|s| s.contains("FA12d") || s.contains("BRAVO"));
                    outbound_log.push(msg.clone());
                    if carries_overflow_session_result {
                        overflow_emission = Some(msg);
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }

        let overflow = overflow_emission.unwrap_or_else(|| {
            panic!(
                "expected an overflow OutboundMessage carrying `_session_result` \
                 metadata via watchers fanout, got {} messages: {:?}",
                outbound_log.len(),
                outbound_log
                    .iter()
                    .map(|m| format!("content={:?} metadata={}", m.content, m.metadata))
                    .collect::<Vec<_>>()
            )
        });

        let session_result = overflow
            .metadata
            .get("_session_result")
            .expect("overflow must carry _session_result metadata");
        assert_eq!(
            session_result.get("role").and_then(|v| v.as_str()),
            Some("assistant"),
            "session_result role must be 'assistant'"
        );
        assert!(
            session_result.get("seq").and_then(|v| v.as_u64()).is_some(),
            "session_result must include committed seq, got {session_result}"
        );
        assert_eq!(
            session_result
                .get("response_to_client_message_id")
                .and_then(|v| v.as_str()),
            Some("client-msg-bravo"),
            "session_result must carry response_to_client_message_id so \
             the web reducer can merge into the optimistic Q2 bubble"
        );
        assert!(
            overflow
                .metadata
                .get("_history_persisted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            "overflow outbound must flag history as persisted"
        );
        // When the channel already streamed the chunks (ApiChannel path),
        // the durable emission omits the content body so non-API channels
        // don't duplicate the bubble and the web doesn't double-render.
        // The full reply is still captured inside `_session_result.content`.
        assert!(
            overflow.content.is_empty() || overflow.content == "FA12d overflow BRAVO answer",
            "expected empty OR full-content body when already_streamed=true, got {:?}",
            overflow.content
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Test that messages within patience threshold are NOT served as overflow.
    #[tokio::test]
    async fn test_speculative_within_patience_serves_both() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 5 warmups + primary (5s) + overflow (fast)
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(200), make_response("w1")),
                (Duration::from_millis(200), make_response("w2")),
                (Duration::from_millis(200), make_response("w3")),
                (Duration::from_millis(200), make_response("w4")),
                (Duration::from_millis(200), make_response("w5")),
                (Duration::from_secs(5), make_response("primary done")),
                (Duration::from_millis(100), make_response("overflow done")),
            ],
        ));

        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-a",
            vec![(Duration::from_millis(100), make_response("unused"))],
        ));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-b",
            vec![(Duration::from_millis(100), make_response("unused"))],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

        // Warm-up
        for i in 0..5 {
            tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        }

        // Send primary (5s)
        tx.send(make_inbound("medium task")).await.unwrap();

        // Send overflow at 2s (within 10s patience) — should still be served
        tokio::time::sleep(Duration::from_secs(2)).await;
        tx.send(make_inbound("quick question")).await.unwrap();

        // Collect responses — should get 2 (both overflow and primary).
        // Skip metadata-only outbounds (the user-message session_result
        // emission added by the #616 fix carries routing metadata in
        // `_session_result` but no body).
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            let is_user_session_result = msg
                .metadata
                .get("_session_result")
                .and_then(|r| r.get("role"))
                .and_then(|v| v.as_str())
                == Some("user");
            if !is_user_session_result {
                responses.push(msg.content);
            }
        }

        assert_eq!(
            responses.len(),
            2,
            "expected 2 responses (overflow + primary), got {}: {:?}",
            responses.len(),
            responses
        );
        // Overflow finishes first (fast), primary finishes second (5s)
        assert!(
            responses.iter().any(|r| r.contains("overflow done")),
            "expected overflow response, got: {:?}",
            responses
        );
        assert!(
            responses.iter().any(|r| r.contains("primary done")),
            "expected primary response, got: {:?}",
            responses
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Test that background results are handled during speculative select loop.
    #[tokio::test]
    async fn test_speculative_handles_background_result() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 5 warmups + 8s primary
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(200), make_response("w1")),
                (Duration::from_millis(200), make_response("w2")),
                (Duration::from_millis(200), make_response("w3")),
                (Duration::from_millis(200), make_response("w4")),
                (Duration::from_millis(200), make_response("w5")),
                (Duration::from_secs(8), make_response("primary done")),
            ],
        ));

        // Router providers (not used in this test — no overflow messages sent)
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("router-a", vec![]));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("router-b", vec![]));

        let (tx, mut rx, handle, _session_mgr) =
            setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

        // Warm-up
        for i in 0..5 {
            tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        }

        // Send primary (8s)
        tx.send(make_inbound("long task")).await.unwrap();

        // Inject background result at 2s (during the speculative select loop)
        tokio::time::sleep(Duration::from_secs(2)).await;
        tx.send(ActorMessage::BackgroundResult {
            task_label: "research".to_string(),
            content: "Background research completed with 5 findings.".to_string(),
            kind: BackgroundResultKind::Report,
            media: vec![],
            originating_thread_id: None,
            ack: None,
        })
        .await
        .unwrap();

        // Collect responses — expect: background notification + primary
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        while responses.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        let has_bg_notification = responses
            .iter()
            .any(|r| r.contains("research") && r.contains("completed"));
        let has_primary = responses.iter().any(|r| r.contains("primary done"));

        assert!(
            has_bg_notification,
            "background result notification not found in: {:?}",
            responses
        );
        assert!(
            has_primary,
            "primary response not found in: {:?}",
            responses
        );

        // Verify background result is in session history
        {
            // Reload from disk (actor writes via its own SessionHandle to per-user dir)
            let handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
            let session = handle.session();
            let report_messages: Vec<_> = session
                .messages
                .iter()
                .filter(|m| {
                    m.role == MessageRole::Assistant
                        && m.content.contains("research")
                        && m.content.contains("completed")
                })
                .collect();
            assert_eq!(
                report_messages.len(),
                1,
                "expected exactly one persisted assistant report result, got: {:?}",
                session.messages
            );
        }

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn test_followup_background_result_notifies_without_rewrite_turn() {
        let dir = tempfile::TempDir::new().unwrap();

        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![(Duration::from_secs(4), make_response("primary done"))],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        tx.send(make_inbound("long task")).await.unwrap();

        tokio::time::sleep(Duration::from_secs(1)).await;
        tx.send(ActorMessage::BackgroundResult {
            task_label: "research".to_string(),
            content: "Background research completed with 5 findings.".to_string(),
            kind: BackgroundResultKind::Report,
            media: vec![],
            originating_thread_id: None,
            ack: None,
        })
        .await
        .unwrap();

        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while responses.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        assert!(
            responses
                .iter()
                .any(|r| r.contains("research") && r.contains("completed")),
            "background notification not found in: {:?}",
            responses
        );
        assert!(
            responses.iter().any(|r| r.contains("primary done")),
            "primary response not found in: {:?}",
            responses
        );

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        let report_messages: Vec<_> = session
            .messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Assistant
                    && m.content.contains("research")
                    && m.content.contains("completed")
            })
            .collect();
        assert!(
            report_messages.len() == 1,
            "expected exactly one persisted assistant report result, got: {:?}",
            session.messages
        );
        assert!(
            session
                .messages
                .iter()
                .all(|m| !m.content.contains("[REWRITE]")),
            "rewrite prompt leaked into session history: {:?}",
            session.messages
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn test_background_notification_persists_media_to_history() {
        let dir = tempfile::TempDir::new().unwrap();
        let media_path = dir.path().join("podcast_full_test.mp3");
        std::fs::write(&media_path, vec![1u8; 4096]).unwrap();

        let agent_llm = Arc::new(DelayedMockProvider::new("agent", vec![]));
        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(ActorMessage::BackgroundResult {
            task_label: "podcast_generate".to_string(),
            content: String::new(),
            kind: BackgroundResultKind::Notification,
            media: vec![media_path.to_string_lossy().to_string()],
            originating_thread_id: None,
            ack: Some(ack_tx),
        })
        .await
        .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_secs(2), ack_rx)
                .await
                .expect("ack timeout")
                .expect("actor ack"),
            "background notification was not persisted"
        );

        let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("outbound timeout")
            .expect("outbound message");
        assert_eq!(
            outbound.media,
            vec![media_path.to_string_lossy().to_string()]
        );

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        let persisted = session.messages.iter().any(|message| {
            message.role == MessageRole::Assistant
                && message.media == vec![media_path.to_string_lossy().to_string()]
        });
        assert!(persisted, "media notification not found in session history");

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// M8.10 follow-up (#649) regression: when a `BackgroundResult` carries
    /// an `originating_thread_id` (the user message's `client_message_id`
    /// from the turn that started the background task), the OutboundMessage
    /// the actor emits MUST stamp that id onto the metadata so the
    /// api_channel routes the wire-side SSE event under the originating
    /// turn — NOT whatever the per-chat sticky map currently holds.
    ///
    /// Drives the production scenario from mini3 (2026-04-29): three user
    /// turns rotate the sticky map, then a long-running deep_research
    /// background task originating in turn A finally finalises. Pre-fix,
    /// the OutboundMessage metadata lacked thread_id and the sticky map
    /// (now pointing at turn C) won; post-fix, the explicit metadata
    /// thread_id always pins the result to turn A.
    #[tokio::test]
    async fn late_tool_result_for_overflow_turn_keeps_originating_thread_id_under_3_user_race() {
        let dir = tempfile::TempDir::new().unwrap();

        // No active turn: the actor is idle, simulating "background task
        // finalises long after the originating turn ended". This is the
        // exact production failure mode.
        let agent_llm = Arc::new(DelayedMockProvider::new("agent", vec![]));
        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        let originating_cmid = "cmid-A-deep-research-originator";

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(ActorMessage::BackgroundResult {
            task_label: "deep_research".to_string(),
            content: "Deep research report on space exploration.".to_string(),
            kind: BackgroundResultKind::Report,
            media: vec![],
            originating_thread_id: Some(originating_cmid.to_string()),
            ack: Some(ack_tx),
        })
        .await
        .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_secs(2), ack_rx)
                .await
                .expect("ack timeout")
                .expect("actor ack"),
            "background result must be persisted"
        );

        let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("outbound timeout")
            .expect("outbound message");

        // The OutboundMessage metadata MUST carry thread_id at the top
        // level so api_channel's `outbound_thread_id(&msg.metadata)` lookup
        // returns Some(originating_cmid) and bypasses the sticky-map
        // fallback. This is the contract the bug fix relies on.
        assert_eq!(
            outbound.metadata.get("thread_id").and_then(|v| v.as_str()),
            Some(originating_cmid),
            "OutboundMessage metadata must carry the originating turn's \
             thread_id so api_channel resolves it via the explicit-metadata \
             path; got metadata = {}",
            outbound.metadata,
        );

        // The embedded `_session_result` ALSO carries thread_id so the
        // wire-side session_result event the api_channel emits has it
        // baked into the message body the web client renders. The v2
        // thread-store keys off `message.thread_id` for routing.
        assert_eq!(
            outbound
                .metadata
                .get("_session_result")
                .and_then(|sr| sr.get("thread_id"))
                .and_then(|v| v.as_str()),
            Some(originating_cmid),
            "embedded _session_result must also carry thread_id so the web \
             client renders the late result under the originating bubble; \
             got metadata = {}",
            outbound.metadata,
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// M8.10 follow-up (#649) PERSISTENCE regression: when a late-arriving
    /// `BackgroundResult` carries an `originating_thread_id`, the PERSISTED
    /// JSONL row for the assistant message must carry that thread_id —
    /// NOT whatever `derive_thread_id_for_new_message`'s "most recent user
    /// in history" fallback would pick.
    ///
    /// PR #664 stamped `thread_id` on the wire-side `OutboundMessage.metadata`
    /// so the live SSE event routed correctly, but `persist_assistant_message`
    /// kept building the message via `Message::assistant(content)` (no
    /// `thread_id`). On the canonical persist path, `add_message_with_seq`
    /// derives `thread_id` from the most recent USER message in history —
    /// for a deep-research result that arrives after Q3, that's Q3's cmid,
    /// not Q1's. Reload from JSONL therefore mis-pairs the assistant under
    /// the WRONG bubble.
    ///
    /// This test pre-seeds three users (Q1/Q2/Q3) into the on-disk session
    /// transcript, sends a late `BackgroundResult` carrying Q1's cmid as
    /// `originating_thread_id`, and verifies the persisted JSONL row picks
    /// up Q1's cmid — proving the new pre-stamp short-circuits the
    /// derivation fallback before it can mis-attribute.
    #[tokio::test]
    async fn late_background_result_persists_with_originating_thread_id_not_derived_from_latest_user()
     {
        let dir = tempfile::TempDir::new().unwrap();
        let session_key = SessionKey::new("cli", "test");

        // Pre-seed three user messages, each with its own client_message_id,
        // through the canonical persist path so the JSONL has the same
        // shape the actor would observe on reload. After this loop the
        // disk transcript is [Q1, Q2, Q3] — Q3 is the "most recent user".
        let originating_cmid = "originating-A-deep-research-Q1";
        let later_cmids = ["B-stocks-Q2", "C-voices-Q3"];
        {
            let user_a = Message::user("Q1: kick off deep research")
                .with_client_message_id(originating_cmid);
            octos_bus::session::persist_message_through_canonical_path(
                dir.path(),
                &session_key,
                user_a,
            )
            .await
            .expect("persist Q1");
            for cmid in later_cmids {
                let user = Message::user(format!("user msg {cmid}")).with_client_message_id(cmid);
                octos_bus::session::persist_message_through_canonical_path(
                    dir.path(),
                    &session_key,
                    user,
                )
                .await
                .expect("persist later user");
            }
        }

        // Spawn the actor — its `SessionHandle::open` will load the three
        // pre-seeded users so the actor's in-memory mirror agrees with disk.
        let agent_llm = Arc::new(DelayedMockProvider::new("agent", vec![]));
        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        // Drive the late background result. `originating_thread_id` is Q1 —
        // pre-fix, derivation in `add_message_with_seq` would pick Q3
        // because Q3 is the most recent user. Post-fix, the persist helper
        // pre-stamps Q1 onto the assistant message so the derivation
        // fallback is skipped.
        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(ActorMessage::BackgroundResult {
            task_label: "deep_research".to_string(),
            content: "Deep research findings for Q1.".to_string(),
            kind: BackgroundResultKind::Report,
            media: vec![],
            originating_thread_id: Some(originating_cmid.to_string()),
            ack: Some(ack_tx),
        })
        .await
        .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_secs(2), ack_rx)
                .await
                .expect("ack timeout")
                .expect("actor ack"),
            "background result must be persisted"
        );

        // Drain one outbound (the wire fanout) to keep the channel from
        // back-pressuring the actor; we already pin wire behaviour in the
        // sibling test so we only need the metadata as a sanity check.
        let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("outbound timeout")
            .expect("outbound message");
        assert_eq!(
            outbound.metadata.get("thread_id").and_then(|v| v.as_str()),
            Some(originating_cmid),
            "wire metadata still must agree with persistence (sibling \
             contract); got metadata = {}",
            outbound.metadata,
        );

        // Reload the session JSONL from disk and find the persisted
        // assistant message. Its `thread_id` MUST equal Q1's cmid — NOT
        // Q3's (which is what the derivation fallback would have chosen).
        let session_handle = SessionHandle::open(dir.path(), &session_key);
        let session = session_handle.session();
        let assistant_messages: Vec<&Message> = session
            .messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Assistant && m.content.contains("Deep research findings")
            })
            .collect();
        assert_eq!(
            assistant_messages.len(),
            1,
            "expected exactly one persisted assistant message for the \
             background result; got messages = {:?}",
            session.messages,
        );
        let persisted_assistant = assistant_messages[0];
        assert_eq!(
            persisted_assistant.thread_id.as_deref(),
            Some(originating_cmid),
            "PERSISTED assistant message must carry originating thread_id \
             (Q1's cmid={originating_cmid:?}) so reload pairs it under the \
             correct user bubble; got thread_id={:?}. The derive fallback \
             would have picked Q3's cmid={:?} which is the bug.",
            persisted_assistant.thread_id,
            later_cmids.last(),
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// M8.10 follow-up (#649): when no `originating_thread_id` is supplied
    /// (legacy callers, pre-fix BackgroundResult senders), the
    /// OutboundMessage metadata must NOT carry a `thread_id` field. This
    /// pins the wire-compat property: callers without a tracked origin
    /// continue to fall through to the api_channel sticky-map fallback,
    /// not surface a phantom empty/null thread_id that would mis-route.
    #[tokio::test]
    async fn legacy_background_result_without_originating_thread_id_omits_metadata_thread_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_llm = Arc::new(DelayedMockProvider::new("agent", vec![]));
        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(ActorMessage::BackgroundResult {
            task_label: "legacy_task".to_string(),
            content: "Legacy result with no origin tracking.".to_string(),
            kind: BackgroundResultKind::Report,
            media: vec![],
            originating_thread_id: None,
            ack: Some(ack_tx),
        })
        .await
        .unwrap();

        let _ = tokio::time::timeout(Duration::from_secs(2), ack_rx).await;
        let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("outbound timeout")
            .expect("outbound message");

        assert!(
            outbound.metadata.get("thread_id").is_none(),
            "legacy callers (originating_thread_id=None) must NOT populate \
             metadata.thread_id — sticky map fallback handles wire compat. \
             got metadata = {}",
            outbound.metadata,
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn test_background_notification_ack_stays_persisted_when_live_fanout_is_closed() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_llm = Arc::new(DelayedMockProvider::new("agent", vec![]));
        let (tx, rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;
        drop(rx);

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(ActorMessage::BackgroundResult {
            task_label: "research".to_string(),
            content: "Background research completed.".to_string(),
            kind: BackgroundResultKind::Report,
            media: vec![],
            originating_thread_id: None,
            ack: Some(ack_tx),
        })
        .await
        .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_secs(2), ack_rx)
                .await
                .expect("ack timeout")
                .expect("actor ack"),
            "background report should still count as persisted when live fanout is unavailable"
        );

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        assert!(
            session
                .messages
                .iter()
                .any(|message| message.role == MessageRole::Assistant
                    && message.content.contains("Background research completed")),
            "persisted background result not found in session history"
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn test_timeout_failure_persists_to_history() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![(Duration::from_millis(250), make_response("late reply"))],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_timeout(agent_llm, Duration::from_millis(50), &dir).await;

        tx.send(make_inbound("slow request")).await.unwrap();

        let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout response")
            .expect("outbound timeout message");
        assert_eq!(outbound.content, "Processing timed out. Please try again.");

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        assert!(
            session
                .messages
                .iter()
                .any(|message| message.role == MessageRole::Assistant
                    && message.content == "Processing timed out. Please try again."),
            "timeout message not found in session history: {:?}",
            session
                .messages
                .iter()
                .map(|message| (message.role, message.content.clone()))
                .collect::<Vec<_>>()
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn test_agent_error_persists_to_history() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_llm = Arc::new(ErrorMockProvider::new("agent", "scripted failure"));

        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        tx.send(make_inbound("cause failure")).await.unwrap();

        let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("error response")
            .expect("outbound error message");
        assert_eq!(outbound.content, "Error: scripted failure");

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        assert!(
            session
                .messages
                .iter()
                .any(|message| message.role == MessageRole::Assistant
                    && message.content == "Error: scripted failure"),
            "error message not found in session history: {:?}",
            session
                .messages
                .iter()
                .map(|message| (message.role, message.content.clone()))
                .collect::<Vec<_>>()
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn test_attachment_hints_do_not_persist_in_session_history() {
        let dir = tempfile::TempDir::new().unwrap();

        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![(
                Duration::from_millis(50),
                make_response("attachment processed"),
            )],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        tx.send(make_attachment_inbound(
            "[Attached files]\n- report.pdf",
            "/tmp/uploads/report.pdf",
        ))
        .await
        .unwrap();

        let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("response timeout")
            .expect("channel closed");

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        let contents = session
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>();

        assert!(
            contents.contains(&"[User sent attachments]"),
            "generic attachment placeholder missing from history: {:?}",
            contents
        );
        assert!(
            contents
                .iter()
                .all(|content| !content.contains("[Attached files]")),
            "transient attachment prompt leaked into history: {:?}",
            contents
        );
        assert!(
            contents
                .iter()
                .all(|content| !content.contains("report.pdf")),
            "attachment filename leaked into history: {:?}",
            contents
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    // ── Queue mode tests ─────────────────────────────────────────────────

    /// Collect mode batches queued messages into one combined prompt.
    #[tokio::test]
    async fn test_queue_mode_collect_batches() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 1st call slow (2s), 2nd call fast
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_secs(2), make_response("first reply")),
                (Duration::from_millis(200), make_response("batched reply")),
            ],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Collect, None, false, &dir).await;

        // Send first message → starts 2s processing
        tx.send(make_inbound("first message")).await.unwrap();

        // Wait for actor to start processing, then queue two more
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(make_inbound("second message")).await.unwrap();
        tx.send(make_inbound("third message")).await.unwrap();

        // Collect responses (expect 2: first reply + batched reply)
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while responses.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        assert_eq!(
            responses.len(),
            2,
            "expected 2 responses (first + batched), got {}: {:?}",
            responses.len(),
            responses
        );

        // Verify session history: second user message should contain batched content
        {
            // Reload from disk (actor writes via its own SessionHandle to per-user dir)
            let handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
            let session = handle.session();
            let user_messages: Vec<&str> = session
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .map(|m| m.content.as_str())
                .collect();
            // First user msg: "first message"
            assert!(
                user_messages.contains(&"first message"),
                "first message not found: {:?}",
                user_messages
            );
            // Second user msg: combined "second message\n---\nQueued #1: third message"
            assert!(
                user_messages
                    .iter()
                    .any(|m| m.contains("second message") && m.contains("third message")),
                "batched message not found: {:?}",
                user_messages
            );
        }

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Steer mode keeps only the newest queued message, discards older ones.
    #[tokio::test]
    async fn test_queue_mode_steer_keeps_newest() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 1st call slow (2s), 2nd call fast
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_secs(2), make_response("first reply")),
                (Duration::from_millis(200), make_response("steered reply")),
            ],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Steer, None, false, &dir).await;

        // Send first message → goes through 500ms coalescing delay, then starts 2s processing
        tx.send(make_inbound("first message")).await.unwrap();

        // Wait for the 500ms coalescing + some processing time, then queue two more.
        // The first message must be past drain_queue before follow-ups arrive,
        // otherwise the coalescing delay will pick them up and steer immediately.
        tokio::time::sleep(Duration::from_millis(800)).await;
        tx.send(make_inbound("second message (discarded)"))
            .await
            .unwrap();
        tx.send(make_inbound("third message (newest)"))
            .await
            .unwrap();

        // Collect responses (expect 2: first + steered)
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        while responses.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        assert_eq!(
            responses.len(),
            2,
            "expected 2 responses, got {}: {:?}",
            responses.len(),
            responses
        );

        // Verify session history: "second message" should NOT appear as a user message
        {
            // Reload from disk (actor writes via its own SessionHandle to per-user dir)
            let handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
            let session = handle.session();
            let user_messages: Vec<&str> = session
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .map(|m| m.content.as_str())
                .collect();
            assert!(
                user_messages.iter().any(|m| m.contains("third message")),
                "steered (newest) message not found: {:?}",
                user_messages
            );
            assert!(
                !user_messages.iter().any(|m| m.contains("second message")),
                "discarded message should not be in history: {:?}",
                user_messages
            );
        }

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Followup mode processes each message individually (no batching).
    #[tokio::test]
    async fn test_queue_mode_followup_sequential() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 3 fast responses
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(100), make_response("reply-1")),
                (Duration::from_millis(100), make_response("reply-2")),
                (Duration::from_millis(100), make_response("reply-3")),
            ],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        // Send 3 messages
        tx.send(make_inbound("msg-a")).await.unwrap();
        tx.send(make_inbound("msg-b")).await.unwrap();
        tx.send(make_inbound("msg-c")).await.unwrap();

        // Collect all 3 responses
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while responses.len() < 3 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        assert_eq!(
            responses.len(),
            3,
            "expected 3 sequential responses, got {}: {:?}",
            responses.len(),
            responses
        );

        // All 3 user messages should be in history individually
        {
            // Reload from disk (actor writes via its own SessionHandle to per-user dir)
            let handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
            let session = handle.session();
            let user_messages: Vec<&str> = session
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .map(|m| m.content.as_str())
                .collect();
            assert!(user_messages.contains(&"msg-a"));
            assert!(user_messages.contains(&"msg-b"));
            assert!(user_messages.contains(&"msg-c"));
        }

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// M8.10-A: the primary-turn `_completion` OutboundMessage MUST carry
    /// `committed_seq` so the API channel can thread it onto the SSE `done`
    /// event. Without this, web clients can't populate `historySeq` on
    /// live-streamed bubbles and they float to the end of the list.
    #[tokio::test]
    async fn primary_turn_completion_metadata_includes_committed_seq() {
        let dir = tempfile::TempDir::new().unwrap();

        // Single fast reply so the primary turn completes quickly and emits
        // `_completion` metadata.
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![(
                Duration::from_millis(50),
                make_response("primary turn reply"),
            )],
        ));
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-a",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-b",
            vec![(Duration::from_millis(500), make_response("unused"))],
        ));

        let status_channel: Arc<dyn octos_bus::Channel> = Arc::new(FakeSseChannel::new("api"));
        let (tx, mut rx, handle, _session_mgr) = setup_speculative_actor_with_indicator(
            agent_llm,
            vec![router_a, router_b],
            status_channel,
            "api",
            &dir,
        )
        .await;

        tx.send(make_inbound_api("hello", "api")).await.unwrap();

        // Drain until we see the primary-turn `_completion` marker.
        let mut completion: Option<OutboundMessage> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => {
                    if msg.metadata.get("_completion").is_some() {
                        completion = Some(msg);
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }

        let completion =
            completion.expect("expected a `_completion` OutboundMessage from primary turn");
        let seq = completion
            .metadata
            .get("committed_seq")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| {
                panic!(
                    "primary-turn _completion metadata must carry `committed_seq`; got {}",
                    completion.metadata
                )
            });
        // Seq is a position index — must point past the user message (seq 0).
        assert!(
            seq >= 1,
            "committed_seq must reference the persisted assistant slot, got {seq}"
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    // ── Auto-escalation tests ────────────────────────────────────────────

    /// Sustained latency degradation triggers auto-escalation to Hedge + Speculative.
    #[tokio::test]
    async fn test_auto_escalation_on_degradation() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 5×100ms warmups + 3×400ms slow (triggers activation at 3× baseline)
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(100), make_response("warm1")),
                (Duration::from_millis(100), make_response("warm2")),
                (Duration::from_millis(100), make_response("warm3")),
                (Duration::from_millis(100), make_response("warm4")),
                (Duration::from_millis(100), make_response("warm5")),
                (Duration::from_millis(400), make_response("slow1")),
                (Duration::from_millis(400), make_response("slow2")),
                (Duration::from_millis(400), make_response("slow3")),
            ],
        ));

        // Router needed for set_mode call during escalation
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("r-a", vec![]));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("r-b", vec![]));
        let router = Arc::new(
            AdaptiveRouter::new(vec![router_a, router_b], &[], AdaptiveConfig::default())
                .with_adaptive_config(AdaptiveMode::Off, false),
        );
        assert_eq!(router.mode(), AdaptiveMode::Off);

        let (tx, mut rx, handle, _) = setup_actor_with_mode(
            agent_llm,
            QueueMode::Followup,
            Some(router.clone()),
            false, // Let warmups establish baseline naturally
            &dir,
        )
        .await;

        // Send all 8 messages (5 warmup + 3 slow) and collect ALL responses.
        // The "⚡" notification is sent BEFORE the reply in process_inbound,
        // so it can arrive interleaved with normal responses.
        let mut all_responses = Vec::new();
        for i in 0..8 {
            let label = if i < 5 {
                format!("warmup {i}")
            } else {
                format!("slow {}", i - 5)
            };
            tx.send(make_inbound(&label)).await.unwrap();
            // Collect all available responses (may be 1 or 2 if "⚡" arrived)
            while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await
            {
                let is_notification = msg.content.contains("⚡");
                all_responses.push(msg.content);
                if !is_notification {
                    break; // Got the actual reply, move to next message
                }
                // If it was the notification, keep reading for the reply
            }
        }

        let found_escalation = all_responses.iter().any(|r| r.contains("⚡"));
        assert!(
            found_escalation,
            "expected ⚡ escalation notification in responses: {:?}",
            all_responses
        );
        assert_eq!(
            router.mode(),
            AdaptiveMode::Hedge,
            "router should be in Hedge mode after escalation"
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Recovery after auto-escalation restores normal mode (Off + Followup).
    #[tokio::test]
    async fn test_auto_deescalation_on_recovery() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 5×100ms warmups + 3×400ms slow + 1×100ms recovery
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(100), make_response("w1")),
                (Duration::from_millis(100), make_response("w2")),
                (Duration::from_millis(100), make_response("w3")),
                (Duration::from_millis(100), make_response("w4")),
                (Duration::from_millis(100), make_response("w5")),
                (Duration::from_millis(400), make_response("s1")),
                (Duration::from_millis(400), make_response("s2")),
                (Duration::from_millis(400), make_response("s3")),
                // Recovery: fast response resets consecutive_slow → deactivation
                (Duration::from_millis(100), make_response("recovered")),
            ],
        ));

        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("r-a", vec![]));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("r-b", vec![]));
        let router = Arc::new(
            AdaptiveRouter::new(vec![router_a, router_b], &[], AdaptiveConfig::default())
                .with_adaptive_config(AdaptiveMode::Off, false),
        );

        let (tx, mut rx, handle, _) = setup_actor_with_mode(
            agent_llm,
            QueueMode::Followup,
            Some(router.clone()),
            false,
            &dir,
        )
        .await;

        // Warmup + degradation (same as escalation test)
        for i in 0..8 {
            tx.send(make_inbound(&format!("msg {i}"))).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;
        }

        // Drain the escalation notification
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) if msg.content.contains("⚡") => break,
                Ok(Some(_)) => continue,
                _ => break,
            }
        }

        // Verify escalated state
        assert_eq!(router.mode(), AdaptiveMode::Hedge);

        // Send recovery message (fast 100ms → resets consecutive_slow to 0)
        // After escalation, queue_mode changed to Speculative internally.
        // The speculative path also records latency and checks deactivation.
        tx.send(make_inbound("recovery ping")).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;

        // Give the actor a moment to process the deactivation
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Router should be back to Off mode
        assert_eq!(
            router.mode(),
            AdaptiveMode::Off,
            "router should revert to Off after recovery"
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    // ── Track B: dispatch profile routing tests ────────────────────────────

    /// Helper: create an ActorRegistry with a minimal ActorFactory for dispatch tests.
    async fn setup_dispatch_registry(
        dir: &tempfile::TempDir,
    ) -> (ActorRegistry, mpsc::Receiver<OutboundMessage>) {
        let provider: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "test",
            (0..20)
                .map(|_| (Duration::from_millis(100), make_response("ok")))
                .collect(),
        ));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let session_mgr = Arc::new(Mutex::new(
            SessionManager::open(&dir.path().join("sessions")).unwrap(),
        ));
        let (out_tx, out_rx) = mpsc::channel(64);
        let tools = octos_agent::ToolRegistry::with_builtins(dir.path());
        let (spawn_tx, _spawn_rx) = mpsc::channel(32);

        let factory = ActorFactory {
            agent_config: AgentConfig {
                save_episodes: false,
                max_iterations: 1,
                ..Default::default()
            },
            llm: provider.clone(),
            llm_strong: provider.clone(),
            llm_for_compaction: provider.clone(),
            memory,
            system_prompt: Arc::new(std::sync::RwLock::new("default prompt".to_string())),
            hooks: None,
            hook_context_template: None,
            data_dir: dir.path().to_path_buf(),
            session_mgr,
            out_tx: out_tx.clone(),
            spawn_inbound_tx: spawn_tx,
            cron_service: None,
            tool_registry_factory: Arc::new(SnapshotToolRegistryFactory::new(tools)),
            pipeline_factory: None,
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(120),
            shutdown: Arc::new(AtomicBool::new(false)),
            cwd: dir.path().to_path_buf(),
            sandbox_config: octos_agent::SandboxConfig::default(),
            provider_policy: None,
            tool_policy: None,
            worker_prompt: None,
            provider_router: None,
            embedder: None,
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            pending_messages: Arc::new(Mutex::new(HashMap::new())),
            queue_mode: QueueMode::Followup,
            adaptive_router: None,
            memory_store: None,
            plugin_dirs: Vec::new(),
            plugin_extra_env: Vec::new(),
            task_query_store: SessionTaskQueryStore::default(),
            subagent_output_router: Arc::new(octos_agent::SubAgentOutputRouter::new(
                dir.path().join("subagent-outputs"),
            )),
        };

        let registry = ActorRegistry::new(
            factory,
            Arc::new(Semaphore::new(10)),
            out_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );

        (registry, out_rx)
    }

    #[tokio::test]
    async fn test_dispatch_routes_by_profile_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut registry, _rx) = setup_dispatch_registry(&dir).await;

        let sk = SessionKey::new("matrix", "!room:localhost");
        let msg = InboundMessage {
            channel: "matrix".to_string(),
            sender_id: "user1".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };

        registry
            .dispatch(DispatchParams {
                message: msg,
                image_media: vec![],
                attachment_media: vec![],
                attachment_prompt: None,
                session_key: sk.clone(),
                reply_channel: "matrix",
                reply_chat_id: "!room:localhost",
                status_indicator: None,
                profile_id: Some("weather"),
                system_prompt_override: Some("You are a weather bot".to_string()),
                sender_user_id: Some("@octos_weather:localhost".to_string()),
            })
            .await;

        let keys = registry.actor_keys();
        assert_eq!(keys.len(), 1);
        assert!(
            keys[0].starts_with("weather:"),
            "dispatch key should start with profile_id, got: {}",
            keys[0]
        );
    }

    #[tokio::test]
    async fn test_dispatch_routes_to_default_profile() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut registry, _rx) = setup_dispatch_registry(&dir).await;

        let sk = SessionKey::new("matrix", "!room:localhost");
        let msg = InboundMessage {
            channel: "matrix".to_string(),
            sender_id: "user1".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };

        registry
            .dispatch(DispatchParams {
                message: msg,
                image_media: vec![],
                attachment_media: vec![],
                attachment_prompt: None,
                session_key: sk,
                reply_channel: "matrix",
                reply_chat_id: "!room:localhost",
                status_indicator: None,
                profile_id: None,
                system_prompt_override: None,
                sender_user_id: None,
            })
            .await;

        let keys = registry.actor_keys();
        assert_eq!(keys.len(), 1);
        assert!(
            keys[0].starts_with("_main:"),
            "dispatch key should start with _main when no profile_id, got: {}",
            keys[0]
        );
    }

    #[tokio::test]
    async fn test_dispatch_profile_and_main_create_separate_actors() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut registry, _rx) = setup_dispatch_registry(&dir).await;

        let sk = SessionKey::new("matrix", "!room:localhost");

        let msg1 = InboundMessage {
            channel: "matrix".to_string(),
            sender_id: "user1".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello weather".to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };
        registry
            .dispatch(DispatchParams {
                message: msg1,
                image_media: vec![],
                attachment_media: vec![],
                attachment_prompt: None,
                session_key: sk.clone(),
                reply_channel: "matrix",
                reply_chat_id: "!room:localhost",
                status_indicator: None,
                profile_id: Some("weather"),
                system_prompt_override: None,
                sender_user_id: None,
            })
            .await;

        let msg2 = InboundMessage {
            channel: "matrix".to_string(),
            sender_id: "user1".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello main".to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };
        registry
            .dispatch(DispatchParams {
                message: msg2,
                image_media: vec![],
                attachment_media: vec![],
                attachment_prompt: None,
                session_key: sk,
                reply_channel: "matrix",
                reply_chat_id: "!room:localhost",
                status_indicator: None,
                profile_id: None,
                system_prompt_override: None,
                sender_user_id: None,
            })
            .await;

        let keys = registry.actor_keys();
        assert_eq!(
            keys.len(),
            2,
            "different profile_ids should create separate actors, got keys: {:?}",
            keys
        );
        assert!(
            keys.iter().any(|k| k.starts_with("weather:")),
            "should have weather-prefixed actor"
        );
        assert!(
            keys.iter().any(|k| k.starts_with("_main:")),
            "should have _main-prefixed actor"
        );
    }

    #[tokio::test]
    async fn test_cancel_matches_profile_scoped_actor_by_session_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut registry, _rx) = setup_dispatch_registry(&dir).await;

        let sk = SessionKey::new("matrix", "!room:localhost");
        let msg = InboundMessage {
            channel: "matrix".to_string(),
            sender_id: "user1".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello weather".to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };
        registry
            .dispatch(DispatchParams {
                message: msg,
                image_media: vec![],
                attachment_media: vec![],
                attachment_prompt: None,
                session_key: sk.clone(),
                reply_channel: "matrix",
                reply_chat_id: "!room:localhost",
                status_indicator: None,
                profile_id: Some("weather"),
                system_prompt_override: None,
                sender_user_id: Some("@octos_weather:localhost".to_string()),
            })
            .await;

        registry.cancel(&sk.to_string()).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        registry.reap_dead_actors();

        assert!(
            registry.actor_keys().is_empty(),
            "cancel should stop the profiled actor when called with the bare session key"
        );
    }

    #[test]
    fn test_sender_metadata_for_system_notice_includes_virtual_user() {
        let metadata = system_notice_metadata(Some("@octos_weather:localhost"));

        assert_eq!(
            metadata
                .get(METADATA_SENDER_USER_ID)
                .and_then(|v| v.as_str()),
            Some("@octos_weather:localhost")
        );
    }

    #[tokio::test]
    async fn test_profile_session_keys_are_persisted_separately() {
        let dir = tempfile::TempDir::new().unwrap();
        let weather_key = SessionKey::with_profile("weather", "matrix", "!room:localhost");
        let news_key = SessionKey::with_profile("news", "matrix", "!room:localhost");

        let mut weather = SessionHandle::open(dir.path(), &weather_key);
        weather
            .add_message(Message::user("weather message"))
            .await
            .unwrap();

        let mut news = SessionHandle::open(dir.path(), &news_key);
        news.add_message(Message::user("news message"))
            .await
            .unwrap();

        let weather = SessionHandle::open(dir.path(), &weather_key);
        let news = SessionHandle::open(dir.path(), &news_key);

        assert_eq!(weather.get_history(10).len(), 1);
        assert_eq!(news.get_history(10).len(), 1);
        assert_eq!(weather.get_history(10)[0].content, "weather message");
        assert_eq!(news.get_history(10)[0].content, "news message");
    }

    #[tokio::test]
    async fn test_persist_child_session_lifecycle_creates_child_history_and_terminal_note() {
        let dir = tempfile::TempDir::new().unwrap();
        let parent = SessionKey::new("api", "parent");
        let child = SessionKey("api:parent#child-task-123".to_string());

        let mut parent_handle = SessionHandle::open(dir.path(), &parent);
        parent_handle
            .add_message(Message::user("Research today’s market moves"))
            .await
            .unwrap();
        parent_handle
            .add_message(Message::assistant("Starting research"))
            .await
            .unwrap();

        let spawned = ChildSessionLifecyclePayload {
            kind: ChildSessionLifecycleKind::Spawned,
            task_id: "task-123".to_string(),
            task_label: "Research report".to_string(),
            instruction: "Research today’s market moves".to_string(),
            parent_session_key: parent.to_string(),
            child_session_key: child.to_string(),
            workflow_kind: Some("deep_research".to_string()),
            current_phase: Some("research".to_string()),
            output_files: Vec::new(),
            failure_action: None,
            error: None,
        };
        assert!(
            persist_child_session_lifecycle(dir.path(), &spawned)
                .await
                .unwrap()
        );

        let completed = ChildSessionLifecyclePayload {
            kind: ChildSessionLifecycleKind::Completed,
            current_phase: Some("deliver_result".to_string()),
            output_files: vec!["/tmp/report.md".to_string()],
            ..spawned.clone()
        };
        assert!(
            persist_child_session_lifecycle(dir.path(), &completed)
                .await
                .unwrap()
        );

        let child_handle = SessionHandle::open(dir.path(), &child);
        let child_session = child_handle.session();
        assert_eq!(child_session.parent_key, Some(parent.clone()));
        assert_eq!(child_session.child_contracts.len(), 1);
        let contract = &child_session.child_contracts[0];
        assert_eq!(contract.task_id, "task-123");
        assert_eq!(
            contract.terminal_state,
            Some(ChildSessionTerminalState::Completed)
        );
        assert_eq!(contract.join_state, Some(ChildSessionJoinState::Joined));
        assert!(contract.joined_at.is_some());
        assert!(
            child_session
                .messages
                .iter()
                .any(|message| message.content == "Starting research"),
            "child session should copy recent parent history"
        );
        assert!(
            child_session
                .messages
                .iter()
                .any(|message| message.role == MessageRole::System
                    && message.content.contains("Background child session created")),
            "child session should record spawn note"
        );
        assert!(
            child_session
                .messages
                .iter()
                .any(|message| message.role == MessageRole::Assistant
                    && message
                        .content
                        .contains("Background task \"Research report\" completed.")
                    && message.content.contains("Join state: joined")
                    && message.content.contains("/tmp/report.md")),
            "child session should record terminal result"
        );

        let parent_handle = SessionHandle::open(dir.path(), &parent);
        let parent_session = parent_handle.session();
        assert_eq!(parent_session.child_contracts.len(), 1);
        assert_eq!(
            parent_session.child_contracts[0].terminal_state,
            Some(ChildSessionTerminalState::Completed)
        );
    }

    #[tokio::test]
    async fn test_persist_child_session_lifecycle_marks_orphaned_terminal_events() {
        let dir = tempfile::TempDir::new().unwrap();
        let parent = SessionKey::new("api", "missing-parent");
        let child = SessionKey("api:missing-parent#child-task-404".to_string());

        let completed = ChildSessionLifecyclePayload {
            kind: ChildSessionLifecycleKind::Completed,
            task_id: "task-404".to_string(),
            task_label: "Orphaned research".to_string(),
            instruction: "Research the missing context".to_string(),
            parent_session_key: parent.to_string(),
            child_session_key: child.to_string(),
            workflow_kind: Some("deep_research".to_string()),
            current_phase: Some("deliver_result".to_string()),
            output_files: vec!["/tmp/orphaned.md".to_string()],
            failure_action: None,
            error: None,
        };

        assert!(
            !persist_child_session_lifecycle(dir.path(), &completed)
                .await
                .unwrap()
        );

        let child_handle = SessionHandle::open(dir.path(), &child);
        let child_session = child_handle.session();
        assert_eq!(child_session.child_contracts.len(), 1);
        assert_eq!(
            child_session.child_contracts[0].join_state,
            Some(ChildSessionJoinState::Orphaned)
        );
        assert_eq!(
            child_session.child_contracts[0].terminal_state,
            Some(ChildSessionTerminalState::Completed)
        );
        assert!(
            child_session
                .messages
                .iter()
                .any(|message| message.content.contains("Join state: orphaned"))
        );
    }

    #[tokio::test]
    async fn test_persist_child_session_lifecycle_repairs_join_when_terminal_arrives_first() {
        let dir = tempfile::TempDir::new().unwrap();
        let parent = SessionKey::new("api", "parent-session");
        let child = SessionKey("api:parent-session#child-task-555".to_string());

        let mut parent_handle = SessionHandle::open(dir.path(), &parent);
        parent_handle
            .add_message(Message::user("Start research"))
            .await
            .unwrap();

        let terminal = ChildSessionLifecyclePayload {
            kind: ChildSessionLifecycleKind::RetryableFailed,
            task_id: "task-555".to_string(),
            task_label: "Research retry".to_string(),
            instruction: "Research with flaky upstream".to_string(),
            parent_session_key: parent.to_string(),
            child_session_key: child.to_string(),
            workflow_kind: Some("deep_research".to_string()),
            current_phase: Some("research".to_string()),
            output_files: Vec::new(),
            failure_action: Some(ChildSessionFailureAction::Retry),
            error: Some("Upstream timed out".to_string()),
        };

        assert!(
            persist_child_session_lifecycle(dir.path(), &terminal)
                .await
                .unwrap()
        );

        let child_handle = SessionHandle::open(dir.path(), &child);
        let child_session = child_handle.session();
        assert_eq!(child_session.parent_key, Some(parent.clone()));
        assert!(
            child_session
                .messages
                .iter()
                .any(|message| message.content == "Start research"),
            "terminal-only join should still seed recent parent history"
        );
        assert_eq!(
            child_session.child_contracts[0].join_state,
            Some(ChildSessionJoinState::Joined)
        );
        assert_eq!(
            child_session.child_contracts[0].terminal_state,
            Some(ChildSessionTerminalState::RetryableFailure)
        );
        assert_eq!(
            child_session.child_contracts[0].failure_action,
            Some(PersistedChildSessionFailureAction::Retry)
        );
        assert!(
            child_session.messages.iter().any(|message| {
                message.content.contains("Failure action: retry")
                    && message
                        .content
                        .contains("Next step: retry from the parent session")
            }),
            "retry policy note missing from terminal child session update"
        );
    }

    #[test]
    fn forced_background_workflow_detects_deep_research() {
        assert_eq!(
            WorkflowKind::detect_forced_background(
                "请对「全球AI代理竞争格局」做一次深度研究，并输出完整报告。"
            ),
            Some(WorkflowKind::DeepResearch)
        );
    }

    #[test]
    fn forced_background_workflow_detects_research_podcast() {
        assert_eq!(
            WorkflowKind::detect_forced_background(
                "用杨幂和窦文涛的声音做一个播客，播报一下北京今日的热点新闻，要求专业冷静。"
            ),
            Some(WorkflowKind::ResearchPodcast)
        );
    }

    #[test]
    fn forced_background_workflow_respects_foreground_override() {
        assert_eq!(
            WorkflowKind::detect_forced_background(
                "请同步等待完成，不要后台。对这个主题做深度研究并直接在这里输出。"
            ),
            None
        );
    }

    /// Speculative-overflow stale-history regression: when the primary turn
    /// finishes quickly (its assistant reply lands in session history before
    /// the deadline), the overflow's history snapshot must reflect that fresh
    /// reply rather than the pre-primary one captured before the primary
    /// agent even started.
    #[tokio::test]
    async fn should_refresh_overflow_history_when_primary_finishes_quickly() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = SessionKey::new("cli", "stale-history-fast");
        let session_handle = Arc::new(Mutex::new(SessionHandle::open(dir.path(), &key)));

        // Pre-primary history: 1 user + 1 assistant exchange.
        {
            let mut handle = session_handle.lock().await;
            handle
                .add_message(Message::user("hi"))
                .await
                .expect("seed user");
            handle
                .add_message(Message::assistant("hello, where to?"))
                .await
                .expect("seed assistant");
        }
        // Simulate process_inbound_speculative: primary user msg saved before
        // primary spawn.
        {
            let mut handle = session_handle.lock().await;
            handle
                .add_message(Message::user("saratoga"))
                .await
                .expect("seed primary user");
        }
        // Pre-primary snapshot (without primary user msg, matching how
        // process_inbound_speculative builds overflow_history).
        let pre_primary_assistant_count = 1;

        // Spawn a task that simulates the primary finishing and its assistant
        // reply landing 200ms later.
        let writer_handle = Arc::clone(&session_handle);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut handle = writer_handle.lock().await;
            let _ = handle
                .add_message(Message::assistant("Saratoga: 72°F sunny"))
                .await;
        });

        let snapshot = wait_for_primary_assistant_reply(
            &session_handle,
            50,
            pre_primary_assistant_count,
            Duration::from_secs(2),
            Duration::from_millis(50),
        )
        .await;

        // Snapshot must include the primary's fresh assistant reply.
        assert!(
            snapshot
                .iter()
                .any(|m| matches!(m.role, MessageRole::Assistant)
                    && m.content.contains("Saratoga")),
            "snapshot must include primary's fresh assistant reply, got {:?}",
            snapshot
                .iter()
                .map(|m| (m.role.as_str(), m.content.as_str()))
                .collect::<Vec<_>>()
        );
    }

    /// Speculative-overflow deadline regression: if the primary turn is still
    /// running when the deadline elapses (no new assistant reply landed), the
    /// helper must fall through with whatever snapshot is available rather
    /// than blocking the overflow indefinitely.
    #[tokio::test]
    async fn should_fall_through_with_pre_primary_history_when_primary_slow() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = SessionKey::new("cli", "stale-history-slow");
        let session_handle = Arc::new(Mutex::new(SessionHandle::open(dir.path(), &key)));

        // Pre-primary history: 1 user + 1 assistant exchange.
        {
            let mut handle = session_handle.lock().await;
            handle
                .add_message(Message::user("hi"))
                .await
                .expect("seed user");
            handle
                .add_message(Message::assistant("hello, where to?"))
                .await
                .expect("seed assistant");
        }
        let pre_primary_assistant_count = 1;

        // No writer task — the helper must time out.
        let started = std::time::Instant::now();
        let snapshot = wait_for_primary_assistant_reply(
            &session_handle,
            50,
            pre_primary_assistant_count,
            Duration::from_millis(300),
            Duration::from_millis(50),
        )
        .await;
        let elapsed = started.elapsed();

        // Helper must exit within ~deadline + one poll interval, not block forever.
        assert!(
            elapsed < Duration::from_millis(700),
            "helper must fall through within deadline, took {}ms",
            elapsed.as_millis()
        );
        // Snapshot equals the pre-primary log (no new assistant landed).
        let snapshot_assistant_count = snapshot
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant))
            .count();
        assert_eq!(
            snapshot_assistant_count,
            pre_primary_assistant_count,
            "no new assistant message should be present, got {:?}",
            snapshot
                .iter()
                .map(|m| (m.role.as_str(), m.content.as_str()))
                .collect::<Vec<_>>()
        );
    }

    /// When the snapshot already has a fresh assistant message at call time,
    /// the helper must return immediately without sleeping.
    #[tokio::test]
    async fn should_return_immediately_when_assistant_already_landed() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = SessionKey::new("cli", "stale-history-immediate");
        let session_handle = Arc::new(Mutex::new(SessionHandle::open(dir.path(), &key)));

        // Seed pre_primary_assistant_count = 0; add 1 assistant before call.
        {
            let mut handle = session_handle.lock().await;
            handle.add_message(Message::user("q")).await.expect("seed");
            handle
                .add_message(Message::assistant("a"))
                .await
                .expect("seed");
        }

        let started = std::time::Instant::now();
        let snapshot = wait_for_primary_assistant_reply(
            &session_handle,
            50,
            0,
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(50),
            "helper must return immediately when condition already true, took {}ms",
            elapsed.as_millis()
        );
        assert_eq!(snapshot.len(), 2);
    }

    // ── M8.9: Runtime failure recovery ─────────────────────────────────────

    #[test]
    fn recovery_prompt_includes_tool_name_and_error() {
        let signal = octos_agent::SpawnOnlyFailureSignal {
            task_id: "task-1".into(),
            tool_name: "fm_tts".into(),
            tool_input: serde_json::json!({"voice": "yangmi"}),
            error_message: "voice 'yangmi' not registered".into(),
            suggested_alternatives: vec![],
            parent_session_key: Some("api:test".into()),
            originating_client_message_id: None,
        };
        let prompt = build_recovery_prompt(&signal);
        assert!(prompt.starts_with("[system-internal]"));
        assert!(prompt.contains("fm_tts"));
        assert!(prompt.contains("voice 'yangmi' not registered"));
        assert!(prompt.contains("path forward"));
    }

    #[test]
    fn recovery_prompt_includes_alternatives_block_when_present() {
        let signal = octos_agent::SpawnOnlyFailureSignal {
            task_id: "task-2".into(),
            tool_name: "fm_tts".into(),
            tool_input: serde_json::Value::Null,
            error_message: "voice missing".into(),
            suggested_alternatives: vec!["vivian".into(), "serena".into(), "longxiang".into()],
            parent_session_key: None,
            originating_client_message_id: None,
        };
        let prompt = build_recovery_prompt(&signal);
        assert!(prompt.contains("Detected alternatives"));
        assert!(prompt.contains("- vivian"));
        assert!(prompt.contains("- serena"));
        assert!(prompt.contains("- longxiang"));
    }

    #[test]
    fn recovery_prompt_omits_alternatives_block_when_empty() {
        let signal = octos_agent::SpawnOnlyFailureSignal {
            task_id: "task-3".into(),
            tool_name: "fm_tts".into(),
            tool_input: serde_json::Value::Null,
            error_message: "internal error".into(),
            suggested_alternatives: vec![],
            parent_session_key: None,
            originating_client_message_id: None,
        };
        let prompt = build_recovery_prompt(&signal);
        assert!(!prompt.contains("Detected alternatives"));
    }

    #[test]
    fn recovery_prompt_includes_tool_input_when_set() {
        let signal = octos_agent::SpawnOnlyFailureSignal {
            task_id: "task-4".into(),
            tool_name: "fm_tts".into(),
            tool_input: serde_json::json!({"voice": "yangmi", "text": "hello"}),
            error_message: "voice missing".into(),
            suggested_alternatives: vec![],
            parent_session_key: None,
            originating_client_message_id: None,
        };
        let prompt = build_recovery_prompt(&signal);
        assert!(prompt.contains("Original input"));
        assert!(prompt.contains("yangmi"));
    }

    #[tokio::test]
    async fn should_enqueue_synthetic_recovery_turn_with_error_message() {
        // End-to-end: a RecoveryHint pushed onto the inbox should drive a
        // primary turn whose user/system content includes the recovery
        // prompt, so the LLM (mock here) sees and responds to it.
        let dir = tempfile::TempDir::new().unwrap();
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![(
                Duration::from_millis(50),
                make_response("acknowledging recovery"),
            )],
        ));
        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm.clone(), QueueMode::Followup, None, false, &dir).await;

        let prompt = build_recovery_prompt(&octos_agent::SpawnOnlyFailureSignal {
            task_id: "task-rh-1".into(),
            tool_name: "fm_tts".into(),
            tool_input: serde_json::json!({"voice": "yangmi"}),
            error_message: "voice 'yangmi' not registered. available: vivian, serena.".into(),
            suggested_alternatives: vec!["vivian".into(), "serena".into()],
            parent_session_key: Some("cli:test".into()),
            originating_client_message_id: None,
        });
        tx.send(ActorMessage::RecoveryHint {
            task_id: "task-rh-1".into(),
            tool_name: "fm_tts".into(),
            prompt,
            originating_client_message_id: None,
        })
        .await
        .unwrap();

        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if !msg.content.is_empty() {
                responses.push(msg.content);
            }
            if !responses.is_empty() {
                break;
            }
        }
        assert!(
            responses
                .iter()
                .any(|c| c.contains("acknowledging recovery")),
            "expected LLM to produce recovery response, got: {:?}",
            responses
        );

        // Verify the synthetic recovery prompt actually landed in history.
        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        let recovery_user_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::User
                    && m.content.contains("[system-internal]")
                    && m.content.contains("fm_tts")
            })
            .collect();
        assert_eq!(
            recovery_user_msgs.len(),
            1,
            "expected exactly one recovery prompt in history, got: {:?}",
            session.messages
        );
        assert!(
            recovery_user_msgs[0].content.contains("vivian"),
            "recovery prompt should include parsed alternatives"
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
    }

    #[tokio::test]
    async fn should_not_enqueue_second_recovery_for_same_task_id() {
        // Two RecoveryHints for the same task_id — only the first should
        // produce a recovery turn. The second is silently dropped via the
        // recovered_tasks claim slot.
        let dir = tempfile::TempDir::new().unwrap();
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(50), make_response("first recovery")),
                (Duration::from_millis(50), make_response("second recovery")),
            ],
        ));
        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        let prompt1 = "[system-internal] first recovery prompt".to_string();
        let prompt2 = "[system-internal] second recovery prompt".to_string();
        tx.send(ActorMessage::RecoveryHint {
            task_id: "task-dup".into(),
            tool_name: "fm_tts".into(),
            prompt: prompt1,
            originating_client_message_id: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(ActorMessage::RecoveryHint {
            task_id: "task-dup".into(),
            tool_name: "fm_tts".into(),
            prompt: prompt2,
            originating_client_message_id: None,
        })
        .await
        .unwrap();

        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if !msg.content.is_empty() {
                responses.push(msg.content);
            }
        }
        // Only the first recovery should have driven an LLM turn.
        let first_seen = responses.iter().any(|c| c.contains("first recovery"));
        let second_seen = responses.iter().any(|c| c.contains("second recovery"));
        assert!(
            first_seen,
            "first recovery should have run: {:?}",
            responses
        );
        assert!(
            !second_seen,
            "second recovery should have been suppressed: {:?}",
            responses
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
    }

    #[tokio::test]
    async fn supervisor_failure_signal_generates_recovery_actor_message_end_to_end() {
        // Full integration: install the failure-signal callback we set up
        // in spawn(), trigger mark_failed, and assert the actor enqueues
        // and processes a RecoveryHint.
        use octos_agent::TaskSupervisor;

        let dir = tempfile::TempDir::new().unwrap();
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![(Duration::from_millis(50), make_response("recovery-handled"))],
        ));
        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        // Mirror the spawn() wiring: a TaskSupervisor whose failure signal
        // dispatches a RecoveryHint into the actor inbox.
        let supervisor = TaskSupervisor::new();
        let recovery_tx = tx.clone();
        supervisor.set_on_failure_signal(move |signal| {
            let prompt = build_recovery_prompt(signal);
            let _ = recovery_tx.try_send(ActorMessage::RecoveryHint {
                task_id: signal.task_id.clone(),
                tool_name: signal.tool_name.clone(),
                prompt,
                originating_client_message_id: signal.originating_client_message_id.clone(),
            });
        });
        let task_id = supervisor.register_with_input(
            "fm_tts",
            "call-int-1",
            Some("cli:test"),
            Some(serde_json::json!({"voice": "yangmi", "text": "hi"})),
        );
        supervisor.mark_failed(
            &task_id,
            "voice 'yangmi' not registered. available: vivian, serena.".into(),
        );

        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if !msg.content.is_empty() {
                responses.push(msg.content);
            }
            if responses.iter().any(|r| r.contains("recovery-handled")) {
                break;
            }
        }
        assert!(
            responses.iter().any(|c| c.contains("recovery-handled")),
            "expected recovery turn to drive an LLM response, got: {:?}",
            responses
        );

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        let prompt_present = session.messages.iter().any(|m| {
            m.role == MessageRole::User
                && m.content.contains("[system-internal]")
                && m.content.contains("fm_tts")
                && m.content.contains("vivian")
        });
        assert!(
            prompt_present,
            "synthetic recovery prompt should be in session history: {:?}",
            session.messages
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
    }

    #[tokio::test]
    async fn recovery_turn_preserves_originating_client_message_id_from_failure_signal() {
        // Issue #738 RED test: when the supervisor emits a
        // `SpawnOnlyFailureSignal` with `originating_client_message_id`,
        // the synthetic recovery turn the actor enqueues MUST persist a
        // user message whose `client_message_id` matches the originating
        // turn's cmid. Pre-fix, `synthetic_recovery_inbound` stamped only
        // `_recovery_turn = true` into the InboundMessage metadata, so
        // `inbound_client_message_id` returned None and `process_inbound`
        // minted a fresh server UUIDv7 — leaving the eventual successful
        // retry's deliverables stranded under an orphan thread_id with no
        // DOM bubble in the SPA.
        use octos_agent::TaskSupervisor;
        const ORIGINATING_CMID: &str = "45756a8f-1234-4abc-8def-cafebabe0001";

        let dir = tempfile::TempDir::new().unwrap();
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![(
                Duration::from_millis(50),
                make_response("recovery-handled-738"),
            )],
        ));
        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        // Mirror the production wiring: the failure-signal callback
        // forwards `signal.originating_client_message_id` onto
        // `ActorMessage::RecoveryHint` so the actor's
        // `synthetic_recovery_inbound` builder can stamp it into metadata.
        let supervisor = TaskSupervisor::new();
        let recovery_tx = tx.clone();
        supervisor.set_on_failure_signal(move |signal| {
            let prompt = build_recovery_prompt(signal);
            let _ = recovery_tx.try_send(ActorMessage::RecoveryHint {
                task_id: signal.task_id.clone(),
                tool_name: signal.tool_name.clone(),
                prompt,
                originating_client_message_id: signal.originating_client_message_id.clone(),
            });
        });

        // Register the failed task with the originating user turn's
        // cmid. The supervisor must thread it through to the failure
        // signal so the recovery turn inherits it.
        let task_id = supervisor.register_with_input_and_cmid(
            "deep_research",
            "call-738",
            Some("cli:test"),
            Some(serde_json::json!({"query": "rust news"})),
            Some(ORIGINATING_CMID.to_string()),
        );
        supervisor.mark_failed(&task_id, "MiniMax 429 rate limited".into());

        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if !msg.content.is_empty() {
                responses.push(msg.content);
            }
            if responses.iter().any(|r| r.contains("recovery-handled-738")) {
                break;
            }
        }
        assert!(
            responses.iter().any(|c| c.contains("recovery-handled-738")),
            "expected recovery turn to drive an LLM response, got: {responses:?}",
        );

        // The decisive assertion: the persisted user message for the
        // recovery turn must carry the originating cmid, NOT a freshly
        // minted server UUIDv7. Pre-fix this was None or a fresh UUID
        // because synthetic_recovery_inbound only stamped
        // `_recovery_turn` and `process_inbound` had nothing to read.
        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        let recovery_msg = session
            .messages
            .iter()
            .find(|m| {
                m.role == MessageRole::User
                    && m.content.contains("[system-internal]")
                    && m.content.contains("deep_research")
            })
            .expect("synthetic recovery user message must be persisted");
        assert_eq!(
            recovery_msg.client_message_id.as_deref(),
            Some(ORIGINATING_CMID),
            "recovery user message must inherit the originating cmid; got {:?}",
            recovery_msg.client_message_id,
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
    }

    #[tokio::test]
    async fn actor_and_channel_persists_get_distinct_seqs_across_paths() {
        // Pins the unified-serialisation contract for `SessionActor` and
        // `ApiChannel::persist_to_session`. Pre-fix, the actor held its own
        // `Arc<Mutex<SessionHandle>>` and called `add_message_with_seq`
        // directly while `ApiChannel::persist_to_session` already routed
        // through `persist_message_through_canonical_path`. The two paths
        // held INDEPENDENT in-memory `messages` Vecs (the actor's grew
        // forever, the channel always opened fresh from disk), so concurrent
        // persists collided: the actor read `len = N` on its local Vec, the
        // channel read disk-len `M`, both returned `seq = X` — duplicate
        // seqs that broke watcher correlation.
        //
        // Post-fix, `persist_assistant_message` also routes through the
        // canonical helper. Both paths contend on the per-key Tokio mutex
        // and observe disk-canonical seqs, so concurrent persists across
        // paths get distinct, monotonic seqs.
        let dir = tempfile::TempDir::new().unwrap();
        let key = SessionKey::new("api", "actor-vs-channel");
        let data_dir = dir.path().to_path_buf();

        // Single shared actor handle (mirrors how `SessionActor` owns ONE
        // long-lived `Arc<Mutex<SessionHandle>>` for the duration of the
        // session). Pre-fix, a series of `add_message_with_seq` calls on
        // this handle increment its local Vec — so seqs returned from this
        // path collide with seqs returned from canonical-helper opens.
        let actor_handle = std::sync::Arc::new(tokio::sync::Mutex::new(SessionHandle::open(
            &data_dir, &key,
        )));

        const TOTAL: usize = 16;
        let mut handles = Vec::with_capacity(TOTAL);

        for i in 0..TOTAL {
            let data_dir = data_dir.clone();
            let key = key.clone();
            let actor_handle = actor_handle.clone();
            handles.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    // "Actor" path — uses the shared `Arc<Mutex<SessionHandle>>`.
                    // Post-fix this call funnels through the canonical helper
                    // so its seq is disk-canonical.
                    let res = persist_assistant_message(
                        &actor_handle,
                        &key,
                        &data_dir,
                        format!("actor-{i}"),
                        vec![],
                        None,
                    )
                    .await;
                    res.map(|p| p.seq)
                } else {
                    // "Channel" path — the canonical helper directly (this is
                    // the same code `ApiChannel::persist_to_session` calls).
                    let assistant = Message::assistant(format!("channel-{i}"));
                    octos_bus::session::persist_message_through_canonical_path(
                        &data_dir, &key, assistant,
                    )
                    .await
                    .ok()
                }
            }));
        }

        let mut seqs = Vec::with_capacity(TOTAL);
        for h in handles {
            let seq = h.await.expect("join").expect("persist returned Some");
            seqs.push(seq);
        }
        seqs.sort_unstable();

        let unique: std::collections::HashSet<usize> = seqs.iter().copied().collect();
        assert_eq!(
            unique.len(),
            TOTAL,
            "actor + channel persists must each receive a distinct seq, got: {seqs:?}"
        );
        assert_eq!(
            seqs,
            (0..TOTAL).collect::<Vec<_>>(),
            "seqs must form a contiguous 0..TOTAL range; got: {seqs:?}"
        );

        // Final disk transcript should hold all TOTAL messages.
        let final_handle = SessionHandle::open(&data_dir, &key);
        assert_eq!(
            final_handle.session().messages.len(),
            TOTAL,
            "all persisted messages must land on disk: {:?}",
            final_handle.session().messages
        );
    }

    // ========================================================================
    // M9-06 — terminal task lifecycle durability under actor inbox backpressure
    // ========================================================================

    fn make_supervisor_task(
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn terminal_task_status_survives_actor_inbox_backpressure() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ActorMessage>(1);
        let data_dir = std::path::PathBuf::from("/tmp/octos-test-data-dir");

        // Pre-fill the inbox so try_send fails.
        tx.try_send(ActorMessage::TaskStatusChanged {
            task_json: "{\"filler\":true}".into(),
        })
        .expect("fill inbox");

        let task = make_supervisor_task(
            "01900000-0000-7000-8000-0000000000aa",
            octos_agent::TaskStatus::Completed,
            octos_agent::TaskRuntimeState::Completed,
        );
        forward_task_status_to_actor_inbox(&tx, &data_dir, &task);

        // Drain the filler so the spawned awaited send can proceed.
        let _ = rx.recv().await.expect("filler");

        tokio::time::advance(std::time::Duration::from_millis(50)).await;

        let delivered = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("terminal must be delivered within timeout")
            .expect("inbox open");
        match delivered {
            ActorMessage::TaskStatusChanged { task_json } => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&task_json).expect("valid json");
                assert_eq!(parsed["id"], "01900000-0000-7000-8000-0000000000aa");
                assert_eq!(parsed["lifecycle_state"], "ready");
            }
            _ => panic!("expected TaskStatusChanged"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_terminal_task_status_drops_under_inbox_backpressure() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ActorMessage>(1);
        let data_dir = std::path::PathBuf::from("/tmp/octos-test-data-dir");

        tx.try_send(ActorMessage::TaskStatusChanged {
            task_json: "{\"filler\":true}".into(),
        })
        .expect("fill inbox");

        let task = make_supervisor_task(
            "01900000-0000-7000-8000-0000000000bb",
            octos_agent::TaskStatus::Running,
            octos_agent::TaskRuntimeState::ExecutingTool,
        );
        forward_task_status_to_actor_inbox(&tx, &data_dir, &task);

        // Drain filler. There must be no durable retry queued behind it.
        let _ = rx.recv().await.expect("filler");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "non-terminal task statuses must not durably retry under backpressure"
        );
    }
}
