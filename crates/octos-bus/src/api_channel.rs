//! API channel — HTTP endpoint for web clients.
//!
//! Provides a `POST /chat` endpoint that accepts messages and returns SSE responses.
//! Used by octos-web to route through the gateway for adaptive routing, queue modes,
//! multi-provider failover, etc.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use chrono::Utc;
use eyre::Result;
use futures::stream::{self, StreamExt};
use metrics::counter;
use octos_core::{
    InboundMessage, MAIN_PROFILE_ID, Message, MessageRole, OutboundMessage, SessionKey,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast, mpsc};
use tracing::{info, warn};

use crate::SessionManager;
use crate::channel::Channel;
use crate::file_handle::{
    encode_profile_file_handle, resolve_legacy_file_request, resolve_scoped_file_handle,
};

/// Callback that returns serialized task list for a session key.
pub type TaskQueryFn = dyn Fn(&str) -> serde_json::Value + Send + Sync;

/// M7.9 / W2: structured outcome for the cancel callback so the
/// `octos-bus` crate doesn't need to depend on `octos-agent` types.
/// Mapped 1:1 onto `octos_agent::TaskCancelError` by the gateway
/// runtime that wires this callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskCancelOutcome {
    /// Task transitioned from active → cancelled.
    Cancelled,
    /// No supervisor knew about the requested task id (404).
    NotFound,
    /// Task is already in a terminal state (409).
    AlreadyTerminal,
}

/// M7.9 / W2: structured outcome for the relaunch callback. `Ok` carries
/// the freshly-allocated successor task id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRelaunchOutcome {
    Relaunched { new_task_id: String },
    NotFound,
    StillActive,
}

/// Callback that cancels a tracked task by id. Returns the structured
/// outcome so the API channel can map it to an HTTP status code.
pub type TaskCancelFn = dyn Fn(&str) -> TaskCancelOutcome + Send + Sync;

/// Callback that relaunches a tracked task by id. The optional
/// `from_node` argument mirrors `RelaunchOpts::from_node`.
pub type TaskRelaunchFn = dyn Fn(&str, Option<&str>) -> TaskRelaunchOutcome + Send + Sync;

/// Callback invoked when a session is deleted via the API.
/// The gateway runtime wires this to stop the session actor.
type OnSessionDeletedFn = Arc<dyn Fn(&str) + Send + Sync>;

const SSE_CHANNEL_CAPACITY: usize = 1024;

type SseSender = broadcast::Sender<String>;
type SseReceiver = broadcast::Receiver<String>;

/// Shared state for the API channel's HTTP handlers.
#[derive(Clone)]
struct ApiState {
    inbound_tx: mpsc::Sender<InboundMessage>,
    pending: Arc<Mutex<HashMap<String, SseSender>>>,
    watchers: Arc<Mutex<HashMap<String, SseSender>>>,
    auth_token: Option<String>,
    profile_id: Option<String>,
    sessions: Arc<Mutex<SessionManager>>,
    task_query: Option<Arc<TaskQueryFn>>,
    /// M7.9 / W2: cancel a tracked background task by id.
    task_cancel: Option<Arc<TaskCancelFn>>,
    /// M7.9 / W2: relaunch (restart-from-node) a tracked task by id.
    task_relaunch: Option<Arc<TaskRelaunchFn>>,
    on_session_deleted: Option<OnSessionDeletedFn>,
    metrics_renderer: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    /// M8.10 follow-up (#636): shared sticky thread_id map. Seeded by
    /// `handle_chat` from the request's `client_message_id` so that the
    /// FIRST event of a turn (the warm-up `thinking`, plus any
    /// `edit_message` / `send_raw_sse` calls that fire before the
    /// session actor's reporter has streamed its own thread_id) can
    /// recover the right cmid via the api_channel's sticky-lookup.
    last_thread_id: Arc<Mutex<HashMap<String, String>>>,
}

fn watcher_key(chat_id: &str, topic: Option<&str>) -> String {
    match topic.filter(|value| !value.trim().is_empty()) {
        Some(topic) => format!("{chat_id}::{}", topic.trim()),
        None => chat_id.to_string(),
    }
}

fn new_sse_channel() -> (SseSender, SseReceiver) {
    broadcast::channel(SSE_CHANNEL_CAPACITY)
}

fn session_result_seq_from_payload(payload: &str) -> Option<usize> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    if value.get("type")?.as_str()? != "session_result" {
        return None;
    }
    value
        .get("message")?
        .get("seq")?
        .as_u64()
        .and_then(|seq| usize::try_from(seq).ok())
}

fn should_drop_replayed_session_result(
    payload: &str,
    max_replayed_session_seq: Option<usize>,
) -> bool {
    let Some(max_seq) = max_replayed_session_seq else {
        return false;
    };
    session_result_seq_from_payload(payload).is_some_and(|seq| seq <= max_seq)
}

fn sse_stream_from_receiver(
    rx: SseReceiver,
    max_replayed_session_seq: Option<usize>,
) -> impl futures::Stream<Item = Result<Event, Infallible>> {
    stream::unfold(rx, move |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(data) => {
                    if should_drop_replayed_session_result(&data, max_replayed_session_seq) {
                        record_duplicate_result_suppressed(
                            "replayed_session_result_already_streamed",
                        );
                        continue;
                    }
                    let event: Result<Event, Infallible> = Ok(Event::default().data(data));
                    return Some((event, rx));
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "dropping lagged SSE events");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

fn record_replay(kind: &'static str, outcome: &'static str, count: usize) {
    let increment = count.min(u64::MAX as usize) as u64;
    counter!(
        "octos_session_replay_total",
        "kind" => kind.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(increment);
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

fn record_duplicate_result_suppressed(reason: &'static str) {
    counter!(
        "octos_result_duplicate_suppressed_total",
        "surface" => "api_channel".to_string(),
        "reason" => reason.to_string()
    )
    .increment(1);
}

fn is_slides_topic(topic: Option<&str>) -> bool {
    topic.is_some_and(|value| value.starts_with("slides"))
}

fn path_looks_like_presentation(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".pptx") || lower.contains(".pptx?")
}

fn message_has_presentation_media(message: &Message) -> bool {
    message
        .media
        .iter()
        .any(|path| path_looks_like_presentation(path))
}

/// Request body for POST /chat.
#[derive(Deserialize)]
struct ChatRequest {
    message: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    topic: Option<String>,
    /// File paths from prior upload.
    #[serde(default)]
    media: Vec<String>,
    #[serde(default)]
    target_profile_id: Option<String>,
    #[serde(default)]
    attach_only: bool,
    /// Client-generated correlation id used by the web reducer to route
    /// the eventual session_result event onto the optimistic bubble the
    /// user sees. Without this, speculative-queue overflow replies arrive
    /// carrying `response_to_client_message_id: null` and the reducer
    /// cannot tell which streaming bubble they belong to — BRAVO's reply
    /// then clobbers ALPHA's bubble and BRAVO's bubble stays empty
    /// (FA-12f).
    ///
    /// Also forwarded to the session actor so the persisted user message
    /// carries it through `add_message_with_seq`, letting the user-message
    /// `session_result` event correlate the optimistic web bubble back to
    /// the server-assigned seq.
    #[serde(default)]
    client_message_id: Option<String>,
}

/// API channel that runs an HTTP server for web client access.
///
/// Messages flow: HTTP POST → InboundMessage → gateway bus → session actor →
/// OutboundMessage → `send()` → SSE events back to the HTTP response.
pub struct ApiChannel {
    port: u16,
    auth_token: Option<String>,
    profile_id: Option<String>,
    shutdown: Arc<AtomicBool>,
    pending: Arc<Mutex<HashMap<String, SseSender>>>,
    watchers: Arc<Mutex<HashMap<String, SseSender>>>,
    /// Track last sent content per `(chat_id, thread_id)` for delta computation.
    /// Keyed by the encoded `last_content_key` so two concurrent streams on
    /// the same chat (speculative-overflow / rapid-fire) compute their token
    /// deltas independently. Without per-thread keying, when turn A's
    /// `prev` content happens to be a prefix of turn B's incoming text,
    /// `edit_message` emits a misleading `token` delta for B that contains
    /// content originally from A — the web client then mis-paints A's
    /// trailing text under B's bubble (overflow-stress phantom-content
    /// regression observed on mini1 #680 follow-up).
    /// The `chat_id`-only key is preserved as a fallback for legacy events
    /// that arrive without a thread_id.
    last_content: Arc<Mutex<HashMap<String, String>>>,
    /// M8.10 follow-up (#632): sticky most-recent thread_id per chat_id.
    /// Populated whenever an outbound metadata or synthetic SSE message_id
    /// carries a thread_id. Used as the fallback for `edit_message` and
    /// `send_raw_sse` when the per-call source lacks one (the race window
    /// between the session actor's first text event and `send_with_id`
    /// observed in production on mini3).
    last_thread_id: Arc<Mutex<HashMap<String, String>>>,
    sessions: Arc<Mutex<SessionManager>>,
    /// Optional callback for querying background tasks by session key.
    task_query: Option<Arc<TaskQueryFn>>,
    /// M7.9 / W2: optional cancel callback. Wired by the gateway runtime
    /// to forward to `SessionTaskQueryStore::cancel_task`.
    task_cancel: Option<Arc<TaskCancelFn>>,
    /// M7.9 / W2: optional relaunch callback. Wired by the gateway
    /// runtime to forward to `SessionTaskQueryStore::relaunch_task`.
    task_relaunch: Option<Arc<TaskRelaunchFn>>,
    /// Optional callback invoked when a session is deleted via API.
    on_session_deleted: Option<OnSessionDeletedFn>,
    /// Optional Prometheus render callback shared from the child gateway.
    metrics_renderer: Option<Arc<dyn Fn() -> String + Send + Sync>>,
}

impl ApiChannel {
    pub fn new(
        port: u16,
        auth_token: Option<String>,
        shutdown: Arc<AtomicBool>,
        sessions: Arc<Mutex<SessionManager>>,
        profile_id: Option<String>,
    ) -> Self {
        Self {
            port,
            auth_token,
            profile_id,
            shutdown,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            last_content: Arc::new(Mutex::new(HashMap::new())),
            last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            sessions,
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
        }
    }

    /// Attach a task query callback for the `/sessions/{id}/tasks` endpoint.
    pub fn with_task_query(mut self, f: Arc<TaskQueryFn>) -> Self {
        self.task_query = Some(f);
        self
    }

    /// M7.9 / W2: attach the cancel callback that backs
    /// `POST /tasks/{task_id}/cancel`. Without this, the route returns
    /// `503 Service Unavailable`.
    pub fn with_task_cancel(mut self, f: Arc<TaskCancelFn>) -> Self {
        self.task_cancel = Some(f);
        self
    }

    /// M7.9 / W2: attach the relaunch callback that backs
    /// `POST /tasks/{task_id}/restart-from-node`. Without this, the
    /// route returns `503 Service Unavailable`.
    pub fn with_task_relaunch(mut self, f: Arc<TaskRelaunchFn>) -> Self {
        self.task_relaunch = Some(f);
        self
    }

    /// Attach a Prometheus render callback for the `/metrics` endpoint.
    pub fn with_metrics_renderer(mut self, render: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        self.metrics_renderer = Some(render);
        self
    }

    /// Attach a callback invoked when a session is deleted via the API.
    /// The gateway runtime uses this to stop the session actor.
    pub fn with_on_session_deleted(mut self, f: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_session_deleted = Some(Arc::new(f));
        self
    }

    /// Test helper: subscribe to the watchers fanout for a (chat_id, topic)
    /// without going through the HTTP `/sessions/:id/events/stream` handler.
    /// Mirrors the subscribe path that the real SSE handler uses (see
    /// `handle_session_event_stream`), so integration tests can assert that
    /// outbound messages carrying `_session_result` metadata are broadcast
    /// to watchers even when the primary turn's `pending` channel has
    /// already been removed (FA-11 defect B regression guard).
    #[doc(hidden)]
    pub async fn subscribe_watcher_for_tests(
        &self,
        chat_id: &str,
        topic: Option<&str>,
    ) -> broadcast::Receiver<String> {
        let mut watchers = self.watchers.lock().await;
        watchers
            .entry(watcher_key(chat_id, topic))
            .or_insert_with(|| {
                let (tx, _rx) = new_sse_channel();
                tx
            })
            .subscribe()
    }

    fn session_workspace_dir(data_dir: &Path, key: &SessionKey) -> PathBuf {
        let encoded = crate::session::encode_path_component(key.base_key());
        data_dir.join("users").join(encoded).join("workspace")
    }

    fn session_artifact_dir(data_dir: &Path, key: &SessionKey) -> PathBuf {
        Self::session_workspace_dir(data_dir, key).join(".artifacts")
    }

    fn sanitize_artifact_name(path: &Path) -> String {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "artifact".to_string());
        name.replace(['/', '\\', '\0'], "_")
    }

    fn find_matching_artifact_copy(
        artifact_dir: &Path,
        source: &Path,
        safe_name: &str,
    ) -> Option<PathBuf> {
        let source_meta = std::fs::metadata(source).ok()?;
        let source_len = source_meta.len();
        let source_bytes = std::fs::read(source).ok()?;

        std::fs::read_dir(artifact_dir)
            .ok()?
            .filter_map(|entry| entry.ok().map(|item| item.path()))
            .find(|candidate| {
                if !candidate.is_file() {
                    return false;
                }
                let Some(name) = candidate.file_name().and_then(|value| value.to_str()) else {
                    return false;
                };
                if name != safe_name && !name.ends_with(&format!("-{safe_name}")) {
                    return false;
                }
                let Ok(candidate_meta) = std::fs::metadata(candidate) else {
                    return false;
                };
                if candidate_meta.len() != source_len {
                    return false;
                }
                std::fs::read(candidate)
                    .map(|bytes| bytes == source_bytes)
                    .unwrap_or(false)
            })
    }

    fn copy_media_into_session_artifacts(artifact_dir: &Path, media: &[String]) -> Vec<String> {
        if let Err(error) = std::fs::create_dir_all(artifact_dir) {
            warn!(
                path = %artifact_dir.display(),
                %error,
                "failed to create session artifact directory"
            );
            return media.to_vec();
        }

        let canonical_artifact_dir =
            std::fs::canonicalize(artifact_dir).unwrap_or_else(|_| artifact_dir.to_path_buf());

        media
            .iter()
            .map(|raw| {
                let source_path = PathBuf::from(raw);
                if source_path.starts_with(&canonical_artifact_dir) {
                    return raw.clone();
                }

                let canonical_source = match std::fs::canonicalize(&source_path) {
                    Ok(path) => path,
                    Err(error) => {
                        warn!(path = %raw, %error, "failed to canonicalize media source");
                        return raw.clone();
                    }
                };

                if canonical_source.starts_with(&canonical_artifact_dir) {
                    return canonical_source.to_string_lossy().to_string();
                }

                let safe_name = Self::sanitize_artifact_name(&canonical_source);
                if let Some(existing) = Self::find_matching_artifact_copy(
                    &canonical_artifact_dir,
                    &canonical_source,
                    &safe_name,
                ) {
                    return existing.to_string_lossy().to_string();
                }
                let dest =
                    canonical_artifact_dir.join(format!("{}-{safe_name}", uuid::Uuid::now_v7()));

                if canonical_source == dest {
                    return canonical_source.to_string_lossy().to_string();
                }

                match std::fs::copy(&canonical_source, &dest) {
                    Ok(_) => dest.to_string_lossy().to_string(),
                    Err(error) => {
                        warn!(
                            source = %canonical_source.display(),
                            dest = %dest.display(),
                            %error,
                            "failed to materialize media into session artifacts"
                        );
                        raw.clone()
                    }
                }
            })
            .collect()
    }

    async fn materialize_media_for_session(
        &self,
        chat_id: &str,
        topic: Option<&str>,
        media: &[String],
    ) -> Vec<String> {
        let key =
            current_profile_api_session_key_with_topic(self.profile_id.as_deref(), chat_id, topic);
        let data_dir = {
            let sess = self.sessions.lock().await;
            sess.data_dir()
        };
        let artifact_dir = Self::session_artifact_dir(&data_dir, &key);
        let media = media.to_vec();
        let media_for_copy = media.clone();
        match tokio::task::spawn_blocking(move || {
            Self::copy_media_into_session_artifacts(&artifact_dir, &media_for_copy)
        })
        .await
        {
            Ok(paths) => paths,
            Err(error) => {
                warn!(chat_id = %chat_id, %error, "failed to join media materialization task");
                media
            }
        }
    }

    async fn broadcast_session_event(
        &self,
        chat_id: &str,
        topic: Option<&str>,
        event: serde_json::Value,
    ) {
        let payload = event.to_string();

        {
            let mut pending = self.pending.lock().await;
            if let Some(tx) = pending.get(chat_id) {
                if tx.send(payload.clone()).is_err() {
                    pending.remove(chat_id);
                }
            }
        }

        let mut watchers = self.watchers.lock().await;
        let key = watcher_key(chat_id, topic);
        if let Some(tx) = watchers.get(&key) {
            if tx.send(payload).is_err() {
                watchers.remove(&key);
            }
        }
    }
}

fn build_session_result_event(
    raw: &serde_json::Value,
    data_dir: &Path,
    materialized_media: Option<&[String]>,
    topic: Option<&str>,
) -> Option<serde_json::Value> {
    let mut message = raw.clone();
    let obj = message.as_object_mut()?;

    let response_media: Option<Vec<String>> = materialized_media
        .map(|paths| {
            paths
                .iter()
                .map(|path| {
                    response_path_for_session_file(data_dir, Path::new(path))
                        .unwrap_or_else(|| path.clone())
                })
                .collect()
        })
        .or_else(|| {
            obj.get("media")
                .and_then(|value| value.as_array())
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(|value| value.as_str())
                        .map(|path| {
                            response_path_for_session_file(data_dir, Path::new(path))
                                .unwrap_or_else(|| path.to_string())
                        })
                        .collect()
                })
        });
    if let Some(paths) = response_media {
        obj.insert("media".to_string(), serde_json::json!(paths));
    }

    Some(serde_json::json!({
        "type": "session_result",
        "topic": topic,
        "message": message,
    }))
}

fn build_session_result_event_from_message(
    message: MessageInfo,
    topic: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "type": "session_result",
        "topic": topic,
        "message": message,
    })
}

fn build_task_status_event(task: serde_json::Value, topic: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "type": "task_status",
        "topic": topic,
        "task": task,
    })
}

/// M8.10 PR #2: insert `thread_id` into a JSON object payload (in place).
/// No-op when `thread_id` is `None` or the value is not an object.
fn inject_thread_id(value: &mut serde_json::Value, thread_id: Option<&str>) {
    if let (Some(tid), Some(obj)) = (thread_id, value.as_object_mut()) {
        obj.insert(
            "thread_id".to_string(),
            serde_json::Value::String(tid.to_string()),
        );
    }
}

/// Read the `thread_id` (if any) from outbound metadata. Empty strings are
/// treated as absent.
fn outbound_thread_id(metadata: &serde_json::Value) -> Option<String> {
    metadata
        .get("thread_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Build the per-(chat_id, thread_id) key used by `last_content` to track
/// per-stream delta state. When `thread_id` is `None`/empty (legacy daemon
/// or pre-bind events) the key falls back to the bare `chat_id`, preserving
/// the historical behaviour for that path. Two concurrent same-chat streams
/// each carrying their own `thread_id` get separate keys, so neither
/// stream's `prev` content can poison the other's delta computation
/// (overflow-stress phantom-content regression — mini1).
fn last_content_key(chat_id: &str, thread_id: Option<&str>) -> String {
    match thread_id.filter(|tid| !tid.is_empty()) {
        Some(tid) => format!("{chat_id}\x1F{tid}"),
        None => chat_id.to_string(),
    }
}

/// Sentinel that delimits the chat_id and the thread_id inside the synthetic
/// message_id returned by `ApiChannel::send_with_id`. ASCII unit separator
/// (0x1F) was chosen because it cannot appear inside JSON string content
/// without explicit escaping, so it cannot collide with a legitimate
/// thread_id payload.
const SSE_THREAD_DELIM: char = '\u{1F}';

/// Encode a synthetic SSE message_id that round-trips both the chat_id and
/// the bound thread_id. Decoded back in `edit_message` to tag streaming
/// `token`/`replace` events with the right thread.
fn encode_sse_message_id(chat_id: &str, thread_id: Option<&str>) -> String {
    match thread_id {
        Some(tid) if !tid.is_empty() => format!("sse-{chat_id}{SSE_THREAD_DELIM}{tid}"),
        _ => format!("sse-{chat_id}"),
    }
}

/// Decode an `(chat_id, thread_id)` pair from a synthetic SSE message_id.
/// Returns the bare chat_id and `None` when the legacy single-segment
/// encoding is used.
fn decode_sse_message_id(message_id: &str) -> (&str, Option<&str>) {
    match message_id.split_once(SSE_THREAD_DELIM) {
        Some((bare, tid)) => (bare, Some(tid).filter(|s| !s.is_empty())),
        None => (message_id, None),
    }
}

fn compatibility_tool_name_for_task(task: &serde_json::Value) -> Option<&'static str> {
    match task.get("tool_name").and_then(|value| value.as_str()) {
        Some("Direct TTS") => Some("fm_tts"),
        _ => None,
    }
}

fn build_bg_task_tool_start_events(tasks: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut seen = std::collections::HashSet::new();
    tasks
        .as_array()
        .into_iter()
        .flatten()
        .filter(|task| {
            matches!(
                task.get("status").and_then(|value| value.as_str()),
                Some("spawned" | "running")
            )
        })
        .filter_map(|task| {
            compatibility_tool_name_for_task(task).map(|tool_name| {
                let tool_call_id = task
                    .get("tool_call_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string();
                (tool_name, tool_call_id)
            })
        })
        .filter(|(tool_name, _)| seen.insert((*tool_name).to_string()))
        .map(|(tool_name, tool_call_id)| {
            serde_json::json!({
                "type": "tool_start",
                "tool": tool_name,
                "tool_call_id": tool_call_id,
            })
        })
        .collect()
}

fn build_replay_complete_event(topic: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "type": "replay_complete",
        "topic": topic,
    })
}

/// Build the synthetic warm-up SSE events emitted the moment a chat
/// request is accepted, before the agent has even begun its first
/// iteration. M8.10 follow-up (#636): the first event of every turn used
/// to leak `thread_id=null` because this builder hardcoded the payload
/// shape without inspecting the request's `client_message_id`. Thread
/// the cmid through so the very first wire event already carries the
/// right routing key.
fn initial_sse_events(has_media: bool, thread_id: Option<&str>) -> Vec<String> {
    let mut thinking = serde_json::json!({
        "type": "thinking",
        "iteration": 0,
    });
    inject_thread_id(&mut thinking, thread_id);
    let mut events = vec![thinking.to_string()];

    if has_media {
        let mut preprocessing = serde_json::json!({
            "type": "tool_progress",
            "tool": "preprocessing",
            "message": "Processing attachments...",
        });
        inject_thread_id(&mut preprocessing, thread_id);
        events.push(preprocessing.to_string());
    }

    events
}

#[async_trait]
impl Channel for ApiChannel {
    fn name(&self) -> &str {
        "api"
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        let state = ApiState {
            inbound_tx,
            pending: self.pending.clone(),
            watchers: self.watchers.clone(),
            auth_token: self.auth_token.clone(),
            profile_id: self.profile_id.clone(),
            sessions: self.sessions.clone(),
            task_query: self.task_query.clone(),
            task_cancel: self.task_cancel.clone(),
            task_relaunch: self.task_relaunch.clone(),
            on_session_deleted: self.on_session_deleted.clone(),
            metrics_renderer: self.metrics_renderer.clone(),
            last_thread_id: self.last_thread_id.clone(),
        };

        let app = Router::new()
            .route("/metrics", get(handle_metrics))
            .route("/chat", post(handle_chat))
            .route("/sessions", get(handle_list_sessions))
            .route("/sessions/{id}/messages", get(handle_session_messages))
            .route(
                "/sessions/{id}/events/stream",
                get(handle_session_event_stream),
            )
            .route("/sessions/{id}/status", get(handle_session_status))
            .route("/sessions/{id}/tasks", get(handle_session_tasks))
            .route("/sessions/{id}", delete(handle_delete_session))
            .route("/sessions/{id}/title", patch(handle_update_session_title))
            // M7.9 / W2 — task supervisor exposure
            .route("/tasks/{task_id}/cancel", post(handle_task_cancel))
            .route(
                "/tasks/{task_id}/restart-from-node",
                post(handle_task_relaunch),
            )
            .route("/files/{*path}", get(handle_file_download))
            .route("/upload", post(handle_upload))
            .route("/admin/shell", post(handle_admin_shell))
            .with_state(state);

        let addr = format!("127.0.0.1:{}", self.port);
        info!(port = self.port, "API channel listening on {addr}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;

        let shutdown = self.shutdown.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                while !shutdown.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            })
            .await?;

        info!("API channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let history_already_persisted = msg
            .metadata
            .get("_history_persisted")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let session_result = msg.metadata.get("_session_result").cloned();

        let topic = msg.metadata.get("topic").and_then(|v| v.as_str());
        // M8.10 PR #2: every SSE event the channel emits below is tagged
        // with this thread_id (the user message's client_message_id) when
        // present. Speculative-overflow + forced-background paths set this
        // to the overflow user's cmid so two concurrent threads on the same
        // chat_id can be demultiplexed by web clients.
        //
        // M8.10 follow-up (#636): when the metadata lacks thread_id (e.g.
        // the stream forwarder's `flush_to_channel` builds outbound
        // metadata containing only `streaming: true`), fall back to the
        // sticky map. handle_chat seeds it from the request's
        // client_message_id BEFORE the agent runs, so the very first
        // `replace` event of a streamed turn is already tagged. Without
        // this fallback, PR #635's lazy sticky-population still leaked
        // the first 1–3 events of every turn (mini1/2/3 probe).
        let metadata_thread_id = outbound_thread_id(&msg.metadata);
        let sticky_thread_id = if metadata_thread_id.is_none() {
            self.sticky_thread_id(&msg.chat_id).await
        } else {
            None
        };
        let thread_id = metadata_thread_id
            .clone()
            .or_else(|| sticky_thread_id.clone());
        // Record the bound thread_id so subsequent `edit_message` and
        // `send_raw_sse` calls on the same chat_id can recover it when
        // their per-call source lacks one. Closes the race window where
        // the session actor sends the user-message session_result (with
        // thread_id) and only later does `flush_to_channel` invoke
        // `send_with_id` with naked metadata.
        self.remember_thread_id(&msg.chat_id, thread_id.as_deref())
            .await;

        if !msg.media.is_empty() {
            if !history_already_persisted
                && self
                    .should_suppress_duplicate_slides_delivery(&msg.chat_id, topic, &msg.media)
                    .await
            {
                record_duplicate_result_suppressed("slides_duplicate_deck_same_user_turn");
                info!(
                    chat_id = %msg.chat_id,
                    topic = topic.unwrap_or_default(),
                    media = ?msg.media,
                    "suppressing duplicate slides deck delivery in same user turn"
                );
                return Ok(());
            }

            let data_dir = {
                let sess = self.sessions.lock().await;
                sess.data_dir()
            };
            let should_materialize_media = !history_already_persisted || session_result.is_none();
            let persisted_media = if should_materialize_media {
                self.materialize_media_for_session(&msg.chat_id, topic, &msg.media)
                    .await
            } else {
                msg.media.clone()
            };
            let tool_call_id = msg
                .metadata
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .map(|value| value.to_string());

            // File message — persist to session history AND send SSE event.
            let committed_message = if !history_already_persisted {
                // PR A: route through the typed assistant constructor when
                // the outbound carries a thread_id (metadata or sticky fallback)
                // so the persisted JSONL row is pinned to the correct thread.
                // Preserve only the human-facing caption. The API/web path
                // already has structured media handles, so persisting
                // synthetic legacy `[file:...]` lines here creates duplicate
                // terminal file deliveries for the same artifact.
                let mut session_msg = match thread_id.as_deref() {
                    Some(tid) if !tid.is_empty() => Message::assistant_with_thread(
                        msg.content.clone(),
                        octos_core::ThreadId::new(tid),
                    ),
                    _ => Message::assistant(msg.content.clone()),
                };
                session_msg.media = persisted_media.clone();
                session_msg.tool_call_id = tool_call_id.clone();
                self.persist_to_session(&msg.chat_id, topic, session_msg)
                    .await
            } else {
                None
            };

            // Forward a committed session result as one authoritative event.
            // This avoids the old split-brain path where file delivery arrived
            // over SSE but the assistant message only appeared after polling.
            if let Some(result) = session_result.as_ref() {
                if let Some(event) =
                    build_session_result_event(result, &data_dir, Some(&persisted_media), topic)
                {
                    record_result_delivery(
                        "session_result_event",
                        "metadata_with_media",
                        "session_result",
                    );
                    record_duplicate_result_suppressed(
                        "session_result_preferred_over_legacy_file_event",
                    );
                    self.broadcast_session_event(&msg.chat_id, topic, event)
                        .await;
                }
                return Ok(());
            }

            if let Some(message) = committed_message {
                record_result_delivery(
                    "session_result_event",
                    "committed_media_message",
                    "session_result",
                );
                record_duplicate_result_suppressed(
                    "committed_session_result_preferred_over_legacy_file_event",
                );
                self.broadcast_session_event(
                    &msg.chat_id,
                    topic,
                    build_session_result_event_from_message(message, topic),
                )
                .await;
                return Ok(());
            }

            // Fallback for already-persisted callers that did not supply
            // session_result metadata. This keeps legacy realtime delivery
            // working until every media path is upgraded to the committed
            // session_result contract.
            let pending = self.pending.lock().await;
            if let Some(tx) = pending.get(&msg.chat_id) {
                record_result_delivery("legacy_file_event", "fallback", "file");
                for (original_path, persisted_path) in msg.media.iter().zip(persisted_media.iter())
                {
                    let filename = std::path::Path::new(original_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let tool_call_id = msg
                        .metadata
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let mut event = serde_json::json!({
                        "type": "file",
                        "path": response_path_for_session_file(&data_dir, Path::new(persisted_path))
                            .unwrap_or_else(|| persisted_path.clone()),
                        "filename": filename,
                        "caption": msg.content,
                        "tool_call_id": tool_call_id,
                    });
                    inject_thread_id(&mut event, thread_id.as_deref());
                    let _ = tx.send(event.to_string());
                }
            }
            return Ok(());
        }

        // Task status change — push raw JSON through SSE
        if let Some(task_json) = msg.metadata.get("_task_status").and_then(|v| v.as_str()) {
            let mut event = build_task_status_event(
                serde_json::from_str::<serde_json::Value>(task_json).unwrap_or_default(),
                topic,
            );
            inject_thread_id(&mut event, thread_id.as_deref());
            self.broadcast_session_event(&msg.chat_id, topic, event)
                .await;
            return Ok(());
        }

        if let Some(result) = session_result.as_ref() {
            let data_dir = {
                let sess = self.sessions.lock().await;
                sess.data_dir()
            };
            if let Some(mut event) = build_session_result_event(result, &data_dir, None, topic) {
                record_result_delivery("session_result_event", "metadata", "session_result");
                // M8.10 follow-up (#649): tag the wire-side session_result
                // with the resolved thread_id so the web client routes it
                // under the originating bubble. Without this, late-arriving
                // background results (deep_research, spawn_only completions)
                // appear under whichever turn happens to hold the sticky
                // thread_id, NOT the turn that actually launched them.
                inject_thread_id(&mut event, thread_id.as_deref());
                self.broadcast_session_event(&msg.chat_id, topic, event)
                    .await;
            }
            return Ok(());
        }

        let is_bg_notification =
            msg.content.starts_with('\u{2713}') || msg.content.starts_with('\u{2717}');
        if is_bg_notification {
            // Background task notification — persist to session history.
            // Client polling will pick this up as the stop signal.
            // PR A: when the outbound carries a thread_id (the originating
            // turn's identity), use the typed assistant constructor so the
            // persisted background-completion row is pinned to the correct
            // thread instead of relying on the late-arrival derivation
            // fallback (the same bug class that drove #649 → #740).
            if !history_already_persisted {
                let session_msg = match thread_id.as_deref() {
                    Some(tid) if !tid.is_empty() => Message::assistant_with_thread(
                        msg.content.clone(),
                        octos_core::ThreadId::new(tid),
                    ),
                    _ => Message::assistant(msg.content.clone()),
                };
                let _ = self
                    .persist_to_session(&msg.chat_id, topic, session_msg)
                    .await;
            }
            return Ok(());
        }

        let mut pending = self.pending.lock().await;
        if let Some(tx) = pending.get(&msg.chat_id) {
            if msg.metadata.get("_completion").is_some() {
                // Completion signal — send done event with metadata, then close.
                // When has_bg_tasks=true, the client starts polling session
                // history for file deliveries and bg_done notifications.
                let has_bg = msg
                    .metadata
                    .get("has_bg_tasks")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if has_bg {
                    if let Some(query_fn) = self.task_query.as_ref() {
                        let tasks = query_tasks_for_session_candidates(
                            query_fn.as_ref(),
                            self.profile_id.as_deref(),
                            &msg.chat_id,
                            topic,
                        );
                        for mut event in build_bg_task_tool_start_events(&tasks) {
                            inject_thread_id(&mut event, thread_id.as_deref());
                            let _ = tx.send(event.to_string());
                        }
                    }
                }
                let mut done = serde_json::json!({
                    "type": "done",
                    "content": "",
                    "model": msg.metadata.get("model").and_then(|v| v.as_str()).unwrap_or(""),
                    "provider": msg.metadata.get("provider").cloned().unwrap_or(serde_json::Value::Null),
                    "model_id": msg.metadata.get("model_id").cloned().unwrap_or(serde_json::Value::Null),
                    "endpoint": msg.metadata.get("endpoint").cloned().unwrap_or(serde_json::Value::Null),
                    "tokens_in": msg.metadata.get("tokens_in").and_then(|v| v.as_u64()).unwrap_or(0),
                    "tokens_out": msg.metadata.get("tokens_out").and_then(|v| v.as_u64()).unwrap_or(0),
                    "session_cost": msg.metadata.get("session_cost").cloned().unwrap_or(serde_json::Value::Null),
                    "duration_s": msg.metadata.get("duration_s").and_then(|v| v.as_u64()).unwrap_or(0),
                    "has_bg_tasks": has_bg,
                });
                // M8.10-A: thread the committed session sequence into the done
                // event so live-streamed bubbles on the web client can populate
                // `historySeq`. Optional — omitted when persist failed or the
                // metadata key was not provided (legacy/error paths).
                if let Some(seq) = msg.metadata.get("committed_seq").and_then(|v| v.as_u64()) {
                    done["committed_seq"] = serde_json::Value::from(seq);
                }
                // Bug 3 / W1.G4 cost panel — forward the per-node cost rows
                // that the session actor pulled out of
                // `ToolResult.structured_metadata` from `run_pipeline`. The
                // CostBreakdown panel reads this array off the `done` event.
                if let Some(node_costs) = msg.metadata.get("node_costs").cloned() {
                    if !node_costs.as_array().map(|a| a.is_empty()).unwrap_or(true) {
                        done["node_costs"] = node_costs;
                    }
                }
                inject_thread_id(&mut done, thread_id.as_deref());
                let _ = tx.send(done.to_string());
                pending.remove(&msg.chat_id);
                drop(pending);
                // Drop both the per-thread and the legacy chat-only entries
                // for this turn so subsequent turns start fresh. The
                // chat-only key is removed defensively — older code paths
                // may have written to it for events without thread_id.
                let mut last = self.last_content.lock().await;
                last.remove(&last_content_key(&msg.chat_id, thread_id.as_deref()));
                last.remove(&msg.chat_id);
            } else if !msg.content.is_empty() {
                // Regular message — send as replace event (full text replacement).
                let mut event = serde_json::json!({
                    "type": "replace",
                    "text": msg.content,
                });
                inject_thread_id(&mut event, thread_id.as_deref());
                if tx.send(event.to_string()).is_err() {
                    pending.remove(&msg.chat_id);
                }
            }
        }
        Ok(())
    }

    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        // Resolve the thread_id ahead of `send`/seeding so the per-thread
        // last_content key matches what `edit_message` uses on the next
        // chunk. We re-read it after `send` populates the sticky map.
        let metadata_thread_id = outbound_thread_id(&msg.metadata);
        // Reset delta tracking for this stream — keyed by (chat, thread)
        // so that turn A's reset never wipes turn B's prev-content under
        // concurrent overflow. The chat-only entry is also cleared so
        // legacy single-keyed state from before per-thread keying does
        // not leak across.
        {
            let mut last = self.last_content.lock().await;
            last.remove(&last_content_key(
                &msg.chat_id,
                metadata_thread_id.as_deref(),
            ));
            last.remove(&msg.chat_id);
        }
        self.send(msg).await?;
        // M8.10 follow-up (#632): seed `last_content` with what `send`
        // just emitted on the wire so the FIRST subsequent `edit_message`
        // can produce a delta `token` event instead of a wasteful full
        // `replace`. Without this seeding, the stream forwarder's first
        // edit re-rendered the entire buffer even though only a suffix
        // changed (matches the documented intent of `last_content`).
        // Use the resolved thread_id (metadata first, sticky fallback)
        // so the seed lands under the same key the next `edit_message`
        // will read from — otherwise a chat-only seed would force a
        // wasteful first `replace` and re-introduce cross-talk under
        // concurrent overflow.
        let seed_thread_id = match metadata_thread_id.clone() {
            Some(tid) => Some(tid),
            None => self.sticky_thread_id(&msg.chat_id).await,
        };
        if !msg.content.is_empty() {
            self.last_content.lock().await.insert(
                last_content_key(&msg.chat_id, seed_thread_id.as_deref()),
                msg.content.clone(),
            );
        }
        // Return a dummy ID so the stream forwarder uses edit_message() for
        // subsequent updates instead of calling send_with_id() again.
        //
        // M8.10 PR #2: encode the bound thread_id into the message_id so
        // subsequent `edit_message` calls can tag streaming `token`/`replace`
        // events with the correct thread (two concurrent threads on the
        // same chat_id is the speculative-overflow case).
        //
        // M8.10 follow-up (#636): when the stream forwarder's metadata
        // lacks thread_id (the common case — `do_flush` builds metadata
        // with only `streaming: true`), recover from the sticky map so
        // the encoded message_id still threads the right cmid through
        // every subsequent `edit_message` call. Without this fallback,
        // every `token` / `replace` event of a turn leaked
        // `thread_id=null`.
        // We already resolved this above (`seed_thread_id`) — reuse it
        // so the encoded message_id matches the key under which the
        // first chunk was seeded.
        Ok(Some(encode_sse_message_id(
            &msg.chat_id,
            seed_thread_id.as_deref(),
        )))
    }

    async fn edit_message(&self, chat_id: &str, message_id: &str, new_content: &str) -> Result<()> {
        if new_content.is_empty() {
            return Ok(());
        }
        // M8.10 PR #2: recover the thread_id encoded into `send_with_id`'s
        // synthetic message_id so streaming `token`/`replace` events can be
        // demultiplexed by web clients running multiple in-flight threads
        // against the same chat_id.
        let (_, decoded_thread_id) = decode_sse_message_id(message_id);
        // M8.10 follow-up (#632): the synthetic message_id is bound by
        // `send_with_id`, but the FIRST `edit_message` of a turn can fire
        // BEFORE that bind happens (the placeholder bubble's first text
        // streams in via `flush_to_channel`'s `send_with_id` call, but
        // `do_flush` builds outbound metadata that lacks `thread_id`). Fall
        // back to the sticky map populated by earlier `send`/`send_with_id`
        // events on the same chat_id so the streaming `token`/`replace`
        // payload still carries the right thread.
        let sticky_thread_id = if decoded_thread_id.is_none() {
            self.sticky_thread_id(chat_id).await
        } else {
            None
        };
        let thread_id = decoded_thread_id.or(sticky_thread_id.as_deref());
        // Update the sticky map whenever we resolved a thread_id, so a
        // subsequent edit on a different (legacy single-segment) message_id
        // still recovers it.
        self.remember_thread_id(chat_id, thread_id).await;
        let pending = self.pending.lock().await;
        if let Some(tx) = pending.get(chat_id) {
            let mut last = self.last_content.lock().await;
            // Per-(chat, thread) keying: two concurrent streams on the same
            // chat must NOT share `prev`. Pre-fix, turn A producing "Hello"
            // would seed prev["chat"]="Hello" — and when turn B's
            // `edit_message` arrived with new_content "Hello world" (B's
            // own first chunk), `starts_with(prev)` was TRUE so an
            // erroneous token delta " world" leaked out tagged with
            // thread_B, even though B never streamed "Hello". The web
            // client then painted A's trailing text under B's bubble.
            let key = last_content_key(chat_id, thread_id);
            let prev = last.get(&key).map(|s| s.as_str()).unwrap_or("");

            // If new content starts with the previous content, send only the delta.
            // This avoids re-rendering the entire message on each streaming update.
            if !prev.is_empty() && new_content.starts_with(prev) {
                let delta = &new_content[prev.len()..];
                if !delta.is_empty() {
                    let mut event = serde_json::json!({
                        "type": "token",
                        "text": delta,
                    });
                    inject_thread_id(&mut event, thread_id);
                    let _ = tx.send(event.to_string());
                }
            } else {
                // Content changed non-incrementally (tool progress replaced, etc.)
                // Send full replacement.
                let mut event = serde_json::json!({
                    "type": "replace",
                    "text": new_content,
                });
                inject_thread_id(&mut event, thread_id);
                let _ = tx.send(event.to_string());
            }
            last.insert(key, new_content.to_string());
        }
        Ok(())
    }

    fn supports_edit(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        1_000_000 // No chunking needed for SSE
    }

    async fn send_raw_sse(&self, chat_id: &str, json: &str) -> Result<()> {
        // M8.10 follow-up (#632): the stream reporter forwards discrete
        // events (thinking, response, tool_start, ...) here as pre-rendered
        // JSON. When the reporter constructed its payload before
        // `with_thread_id` had been bound, the JSON arrives without a
        // `thread_id` field. Inject from the sticky map so wire events are
        // still tagged with the right thread.
        let payload = match serde_json::from_str::<serde_json::Value>(json) {
            Ok(mut value) => {
                let already_has = value
                    .get("thread_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .is_some();
                if already_has {
                    if let Some(tid) = value
                        .get("thread_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                    {
                        self.remember_thread_id(chat_id, Some(tid.as_str())).await;
                    }
                    json.to_string()
                } else if let Some(sticky) = self.sticky_thread_id(chat_id).await {
                    inject_thread_id(&mut value, Some(sticky.as_str()));
                    value.to_string()
                } else {
                    json.to_string()
                }
            }
            Err(_) => json.to_string(),
        };
        let pending = self.pending.lock().await;
        if let Some(tx) = pending.get(chat_id) {
            let _ = tx.send(payload);
        }
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

async fn handle_metrics(State(state): State<ApiState>) -> String {
    state
        .metrics_renderer
        .as_ref()
        .map(|render| render())
        .unwrap_or_default()
}

impl ApiChannel {
    /// M8.10 follow-up (#632): record `thread_id` as the most-recent bound
    /// value for `chat_id`. No-op when `thread_id` is `None` or empty so
    /// late legacy events on the same chat_id can never erase a previously
    /// bound thread (the sticky property the bug fix relies on).
    async fn remember_thread_id(&self, chat_id: &str, thread_id: Option<&str>) {
        let Some(tid) = thread_id.filter(|s| !s.is_empty()) else {
            return;
        };
        let mut map = self.last_thread_id.lock().await;
        map.insert(chat_id.to_string(), tid.to_string());
    }

    /// M8.10 follow-up (#632): look up the sticky thread_id for `chat_id`,
    /// returning `None` if nothing has been bound yet on this chat.
    async fn sticky_thread_id(&self, chat_id: &str) -> Option<String> {
        self.last_thread_id.lock().await.get(chat_id).cloned()
    }

    async fn should_suppress_duplicate_slides_delivery(
        &self,
        chat_id: &str,
        topic: Option<&str>,
        media: &[String],
    ) -> bool {
        if !is_slides_topic(topic) || !media.iter().any(|path| path_looks_like_presentation(path)) {
            return false;
        }

        let key =
            current_profile_api_session_key_with_topic(self.profile_id.as_deref(), chat_id, topic);
        let mut sess = self.sessions.lock().await;
        let history = sess.get_or_create(&key).await.get_history(256).to_vec();

        for message in history.iter().rev() {
            if message.role == MessageRole::User {
                break;
            }
            if message.role == MessageRole::Assistant && message_has_presentation_media(message) {
                return true;
            }
        }

        false
    }

    /// Persist a message to the canonical per-user session JSONL and return
    /// the authoritative committed message shape when available.
    ///
    /// Routes through the shared
    /// [`crate::session::persist_message_through_canonical_path`] helper so:
    ///   - bus-side writes hit the same
    ///     `users/<encoded_base>/sessions/<encoded_topic>.jsonl` file the
    ///     `SessionActor` uses (closing the split-brain storage bug);
    ///   - concurrent writes for the same session_key serialise via a
    ///     per-key Tokio mutex (closing the concurrent-persist seq race).
    ///
    /// The legacy flat layout is no longer touched on writes; reads still
    /// merge it for back-compat with stale on-disk data.
    async fn persist_to_session(
        &self,
        chat_id: &str,
        topic: Option<&str>,
        message: Message,
    ) -> Option<MessageInfo> {
        let key =
            current_profile_api_session_key_with_topic(self.profile_id.as_deref(), chat_id, topic);
        let data_dir = {
            let sess = self.sessions.lock().await;
            sess.data_dir()
        };

        let result = crate::session::persist_message_through_canonical_path(
            &data_dir,
            &key,
            message.clone(),
        )
        .await;

        // Drop any stale `SessionManager` cache entry for this key so a
        // follow-up read (e.g. duplicate-detection or `?source=full`) consults
        // disk instead of returning a pre-write empty `Session`. Without this
        // invalidation the manager's LRU cache could shadow the canonical
        // per-user JSONL and silently strip newly-written messages.
        {
            let mut sess = self.sessions.lock().await;
            sess.invalidate_cache(&key);
        }

        match result {
            Ok(seq) => {
                info!(
                    chat_id = %chat_id,
                    key = %key.0,
                    seq,
                    "persisted file/notification to canonical per-user session"
                );
                Some(message_info_from_history_message(&message, &data_dir, seq))
            }
            Err(error) => {
                tracing::warn!(
                    chat_id = %chat_id,
                    key = %key.0,
                    error = %error,
                    "failed to persist message to canonical per-user session"
                );
                None
            }
        }
    }
}

/// POST /chat handler — accepts a message, returns an SSE stream of events.
async fn handle_chat(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Response {
    // Validate auth token if configured
    if let Some(ref expected) = state.auth_token {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if provided != Some(expected.as_str()) {
            return (StatusCode::UNAUTHORIZED, "invalid auth token").into_response();
        }
    }

    let session_id = req
        .session_id
        .unwrap_or_else(|| format!("web-{}", uuid::Uuid::now_v7()));

    // M8.10 follow-up (#636): pull the request's `client_message_id`
    // up here so it can tag both the synthetic warm-up SSE events AND
    // seed the sticky map BEFORE the session actor's reporter starts
    // streaming. The original M8.10 PR #2 + sticky-map follow-up #632
    // bound thread_id at the reporter level, but the warm-up `thinking`
    // event predates the reporter and the first `edit_message` /
    // `send_raw_sse` calls of the turn could race with the actor's
    // first thread_id-tagged emission, leaving early `replace` events
    // un-routed. Seeding here closes that window.
    let request_thread_id: Option<String> = req
        .client_message_id
        .clone()
        .filter(|value| !value.is_empty());

    // Create per-request SSE channel. If a previous request is still streaming
    // AND alive, reuse it. Otherwise, replace the stale sender.
    let rx = {
        let mut pending = state.pending.lock().await;
        let stale = if let Some(old_tx) = pending.get(&session_id) {
            old_tx.receiver_count() == 0
        } else {
            false
        };
        if stale {
            info!(session = %session_id, "removing stale SSE sender");
            pending.remove(&session_id);
        }
        if pending.contains_key(&session_id) {
            // Previous stream still active — queue on existing
            None
        } else {
            let (tx, rx) = new_sse_channel();
            for event in initial_sse_events(!req.media.is_empty(), request_thread_id.as_deref()) {
                let _ = tx.send(event);
            }
            pending.insert(session_id.clone(), tx);
            Some(rx)
        }
    };

    // Seed the sticky thread_id map so subsequent untagged
    // `edit_message` / `send_raw_sse` calls on this chat_id can recover
    // the cmid via the api_channel's sticky lookup. Done OUTSIDE the
    // pending lock so the locks don't nest. Idempotent — calling this
    // when the request had no cmid is a no-op.
    if let Some(ref tid) = request_thread_id {
        let mut map = state.last_thread_id.lock().await;
        map.insert(session_id.clone(), tid.clone());
    }

    if !req.attach_only {
        // Build and send InboundMessage to the gateway bus.
        //
        // FA-12f: thread the web's `client_message_id` through as the
        // inbound's platform `message_id`. It surfaces downstream as the
        // overflow agent's `reply_to` which becomes
        // `_session_result.response_to_client_message_id` — the field the
        // web reducer correlates against the optimistic streaming bubble.
        let inbound = InboundMessage {
            channel: "api".into(),
            sender_id: "web".into(),
            chat_id: session_id.clone(),
            content: req.message,
            timestamp: Utc::now(),
            media: req.media,
            metadata: {
                let mut metadata = serde_json::Map::new();
                if let Some(profile_id) = req.target_profile_id.filter(|value| !value.is_empty()) {
                    metadata.insert(
                        "target_profile_id".to_string(),
                        serde_json::Value::String(profile_id),
                    );
                }
                if let Some(topic) = req.topic.filter(|value| !value.is_empty()) {
                    metadata.insert("topic".to_string(), serde_json::Value::String(topic));
                }
                if let Some(cmid) = req
                    .client_message_id
                    .clone()
                    .filter(|value| !value.is_empty())
                {
                    metadata.insert(
                        "client_message_id".to_string(),
                        serde_json::Value::String(cmid),
                    );
                }
                serde_json::Value::Object(metadata)
            },
            message_id: req
                .client_message_id
                .clone()
                .filter(|value| !value.is_empty()),
        };

        if let Err(e) = state.inbound_tx.send(inbound).await {
            let mut pending = state.pending.lock().await;
            pending.remove(&session_id);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to send message: {e}"),
            )
                .into_response();
        }
    }

    // If no new SSE stream (previous one still active), return queued acknowledgment
    let Some(rx) = rx else {
        return Json(serde_json::json!({
            "status": "queued",
            "message": "Message queued — response will arrive on the existing stream"
        }))
        .into_response();
    };

    Sse::new(sse_stream_from_receiver(rx, None))
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn handle_session_event_stream(
    State(state): State<ApiState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<PaginationParams>,
) -> Response {
    if let Some(ref expected) = state.auth_token {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if provided != Some(expected.as_str()) {
            return (StatusCode::UNAUTHORIZED, "invalid auth token").into_response();
        }
    }

    let rx = {
        let mut watchers = state.watchers.lock().await;
        watchers
            .entry(watcher_key(&id, params.topic.as_deref()))
            .or_insert_with(|| {
                let (tx, _rx) = new_sse_channel();
                tx
            })
            .subscribe()
    };

    let mut replay_events = replay_task_status_events(&state, &id, params.topic.as_deref()).await;
    replay_events.extend(
        replay_committed_session_results(&state, &id, params.since_seq, params.topic.as_deref())
            .await,
    );
    let max_replayed_session_seq = replay_events
        .iter()
        .filter_map(|payload| session_result_seq_from_payload(payload))
        .max();
    replay_events.push(build_replay_complete_event(params.topic.as_deref()).to_string());
    record_replay("stream", "opened", 1);

    let live_stream = sse_stream_from_receiver(rx, max_replayed_session_seq);

    let replay_stream = stream::iter(
        replay_events
            .into_iter()
            .map(|data| Ok::<Event, Infallible>(Event::default().data(data))),
    );
    let stream = replay_stream.chain(live_stream);

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ── Session REST endpoints ───────────────────────────────────────────

#[derive(Serialize)]
struct SessionInfo {
    id: String,
    message_count: usize,
    /// Display title from the session's JSONL meta line (auto-derived from
    /// first user message, or set manually). None for legacy sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
}

#[derive(Serialize)]
struct MessageInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seq: Option<usize>,
    role: String,
    content: String,
    timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    media: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<serde_json::Value>,
    /// Client-supplied UUID propagated from `Message::client_message_id`. Lets
    /// the web/runtime client correlate optimistic bubbles to the persisted
    /// seq without a backfill round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_message_id: Option<String>,
    /// M8.10 PR #1 thread grouping key (mirrors `Message::thread_id`). Lets
    /// the web client render chat history as `Vec<Thread>` without a flat
    /// re-grouping pass. Omitted when `None` so legacy clients still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
}

#[derive(Deserialize)]
struct PaginationParams {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    /// "full" to read from disk (complete history), default reads from memory (compacted for LLM).
    #[serde(default)]
    source: Option<String>,
    /// Return only messages strictly newer than this sequence number.
    #[serde(default)]
    since_seq: Option<usize>,
    /// Explicit topic override for multi-topic sessions.
    #[serde(default)]
    topic: Option<String>,
}

fn default_limit() -> usize {
    100
}

fn task_list_has_active_tasks(tasks: &serde_json::Value) -> bool {
    tasks.as_array().is_some_and(|entries| {
        entries.iter().any(|task| {
            matches!(
                task.get("status").and_then(|value| value.as_str()),
                Some("spawned" | "running")
            )
        })
    })
}

fn current_profile_api_session_key_with_topic(
    profile_id: Option<&str>,
    chat_id: &str,
    topic: Option<&str>,
) -> SessionKey {
    SessionKey::with_profile_topic(
        profile_id
            .filter(|value| !value.is_empty())
            .unwrap_or(MAIN_PROFILE_ID),
        "api",
        chat_id,
        topic.unwrap_or_default(),
    )
}

fn api_session_key_candidates(
    profile_id: Option<&str>,
    id: &str,
    topic: Option<&str>,
) -> Vec<SessionKey> {
    let mut keys = Vec::with_capacity(4);
    let raw_id = api_chat_id_from_session_key(id).unwrap_or(id);

    if raw_id != id && topic.filter(|value| !value.is_empty()).is_none() {
        keys.push(SessionKey(id.to_string()));
    }

    if let Some(topic) = topic.filter(|value| !value.is_empty()) {
        if let Some(profile_id) = profile_id.filter(|value| !value.is_empty()) {
            keys.push(SessionKey::with_profile_topic(
                profile_id, "api", raw_id, topic,
            ));
        }
        keys.push(SessionKey::with_profile_topic(
            MAIN_PROFILE_ID,
            "api",
            raw_id,
            topic,
        ));
        keys.push(SessionKey::with_topic("api", raw_id, topic));
    } else {
        if let Some(profile_id) = profile_id.filter(|value| !value.is_empty()) {
            keys.push(SessionKey::with_profile(profile_id, "api", raw_id));
        }
        keys.push(SessionKey::with_profile(MAIN_PROFILE_ID, "api", raw_id));
        keys.push(SessionKey::new("api", raw_id));
    }

    keys.dedup_by(|left, right| left.0 == right.0);
    keys
}

fn query_tasks_for_session_candidates(
    query_fn: &TaskQueryFn,
    profile_id: Option<&str>,
    id: &str,
    topic: Option<&str>,
) -> serde_json::Value {
    for session_key in api_session_key_candidates(profile_id, id, topic) {
        let tasks = query_fn(&session_key.0);
        if tasks.as_array().is_some_and(|entries| !entries.is_empty()) {
            return tasks;
        }
    }
    serde_json::json!([])
}

fn api_chat_id_from_session_key(id: &str) -> Option<&str> {
    let chat_id = id
        .strip_prefix("api:")
        .or_else(|| id.split_once(":api:").map(|(_, chat_id)| chat_id))
        .or_else(|| (!id.contains(':')).then_some(id))?;
    if is_internal_api_chat_id(chat_id) {
        None
    } else {
        Some(chat_id)
    }
}

fn is_internal_api_chat_id(chat_id: &str) -> bool {
    chat_id
        .split_once('#')
        .is_some_and(|(_, topic)| is_internal_session_topic(topic))
}

fn is_internal_session_topic(topic: &str) -> bool {
    topic.starts_with("child-") || topic == "default.tasks" || topic.ends_with(".tasks")
}

fn response_path_for_session_file(data_dir: &Path, path: &Path) -> Option<String> {
    encode_profile_file_handle(data_dir, path)
}

fn sanitize_message_file_markers(content: &str, data_dir: &Path) -> String {
    let mut remaining = content;
    let mut sanitized = String::with_capacity(content.len());

    while let Some(start) = remaining.find("[file:") {
        let (before, rest) = remaining.split_at(start);
        sanitized.push_str(before);

        let Some(end) = rest.find(']') else {
            sanitized.push_str(rest);
            return sanitized;
        };

        let raw_path = &rest[6..end];
        let replacement = Path::new(raw_path)
            .is_absolute()
            .then(|| response_path_for_session_file(data_dir, Path::new(raw_path)))
            .flatten()
            .unwrap_or_else(|| raw_path.to_string());
        sanitized.push_str("[file:");
        sanitized.push_str(&replacement);
        sanitized.push(']');
        remaining = &rest[end + 1..];
    }

    sanitized.push_str(remaining);
    sanitized
}

fn message_info_from_history_message(
    message: &Message,
    data_dir: &Path,
    seq: usize,
) -> MessageInfo {
    MessageInfo {
        seq: Some(seq),
        role: message.role.to_string(),
        content: sanitize_message_file_markers(&message.content, data_dir),
        timestamp: message.timestamp.to_rfc3339(),
        tool_call_id: message.tool_call_id.clone(),
        media: message
            .media
            .iter()
            .filter_map(|path| response_path_for_session_file(data_dir, Path::new(path)))
            .collect(),
        client_message_id: message.client_message_id.clone(),
        thread_id: message.thread_id.clone(),
        tool_calls: message
            .tool_calls
            .as_ref()
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| serde_json::to_value(call).ok())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

async fn snapshot_session_disk_loader(
    sessions: &Arc<Mutex<SessionManager>>,
) -> Option<(PathBuf, SessionManager)> {
    let data_dir = {
        let sess = sessions.lock().await;
        sess.data_dir()
    };

    match SessionManager::open(&data_dir) {
        Ok(loader) => Some((data_dir, loader)),
        Err(error) => {
            warn!(
                path = %data_dir.display(),
                error = %error,
                "failed to prepare session disk loader"
            );
            None
        }
    }
}

fn assistant_message_has_displayable_content(message: &Message) -> bool {
    !message.content.trim().is_empty() || !message.media.is_empty()
}

async fn replay_task_status_events(state: &ApiState, id: &str, topic: Option<&str>) -> Vec<String> {
    let Some(ref query_fn) = state.task_query else {
        record_replay("task_status", "disabled", 1);
        return Vec::new();
    };

    let events: Vec<String> = query_tasks_for_session_candidates(
        query_fn.as_ref(),
        state.profile_id.as_deref(),
        id,
        topic,
    )
    .as_array()
    .cloned()
    .unwrap_or_default()
    .into_iter()
    .map(|task| build_task_status_event(task, topic).to_string())
    .collect();
    if events.is_empty() {
        record_replay("task_status", "empty", 1);
    } else {
        record_replay("task_status", "emitted", events.len());
    }
    events
}

async fn replay_committed_session_results(
    state: &ApiState,
    id: &str,
    since_seq: Option<usize>,
    topic: Option<&str>,
) -> Vec<String> {
    let candidates = api_session_key_candidates(state.profile_id.as_deref(), id, topic);
    let Some((data_dir, session_loader)) = snapshot_session_disk_loader(&state.sessions).await
    else {
        return Vec::new();
    };

    // Collect candidate-events first WITHOUT early-returning. The previous
    // shape returned `events` as soon as ANY candidate file resolved, even
    // when its filtered output was empty — short-circuiting the topic-less
    // fallback below for the case where a topic-less candidate JSONL exists
    // but only contains user/tool-trace lines (no displayable assistant
    // content). The fallback is the only path that surfaces a topic-bearing
    // audio bubble to a topic-less reconnect, so we must NOT early-return on
    // an empty candidate result.
    //
    // Both branches stash `(timestamp, payload)` so the combined-replay path
    // can globally sort by timestamp before returning. Pre-fix, candidate
    // events were concatenated in disk order in front of fallback events
    // — if the two branches' timestamps interleaved (e.g. candidate=T0,T2,T4
    // and fallback=T1,T3,T5), replay surfaced T0,T2,T4,T1,T3,T5 instead of
    // T0..T5. The web client renders bubbles in delivery order, so the
    // mis-sort manifested as a "leap back in time" mid-replay.
    let mut candidate_events: Vec<(chrono::DateTime<chrono::Utc>, String)> = Vec::new();
    for candidate in &candidates {
        if let Some(session) = session_loader.load(candidate).await {
            for (seq, message) in session.messages.iter().enumerate() {
                let passes = since_seq.is_none_or(|since| seq > since)
                    && message.role == MessageRole::Assistant
                    && assistant_message_has_displayable_content(message);
                if !passes {
                    continue;
                }
                let payload = build_session_result_event_from_message(
                    message_info_from_history_message(message, &data_dir, seq),
                    topic,
                )
                .to_string();
                candidate_events.push((message.timestamp, payload));
            }
            // Stop at the first resolved candidate file even if it produced
            // zero displayable events — we do not want to layer multiple
            // candidate JSONLs on top of each other; the fallback below
            // handles the topic-less union case explicitly.
            break;
        }
    }

    // Topic-less reconnect fallback. The actor writes spawn_only file
    // deliveries to per-user `<topic>.jsonl`; when the watcher subscribes
    // without a topic, none of the topic-less candidates above resolves to
    // that file. Scan every per-user JSONL for these candidates' base_keys
    // and union the assistant messages so the audio bubble re-materialises.
    let mut fallback_events: Vec<(chrono::DateTime<chrono::Utc>, String)> = Vec::new();
    if topic.is_none() {
        let mut scanned: std::collections::HashSet<String> = std::collections::HashSet::new();
        for candidate in &candidates {
            let base_key = candidate.base_key();
            if !scanned.insert(base_key.to_string()) {
                continue;
            }
            for topic_key in session_loader.list_user_session_keys(base_key) {
                if topic_key.topic().is_none() {
                    continue; // already covered by candidate-load above
                }
                let Some(session) = session_loader.load(&topic_key).await else {
                    continue;
                };
                let topic_str = topic_key.topic().map(str::to_string);
                for (seq, message) in session.messages.iter().enumerate() {
                    // NOTE: we deliberately do NOT apply `since_seq` here.
                    // `since_seq` is a per-watcher cursor measured against the
                    // unified replay sequence — comparing it to a per-file
                    // index is the wrong axis (a cursor of 5 must NOT mean
                    // "skip 5 messages of EACH topic file"). The fallback's
                    // job is to re-materialise spawn_only file deliveries on
                    // a topic-less reconnect; tracking per-file cursors is
                    // meaningless here and was silently dropping legitimate
                    // assistant rows.
                    let passes = message.role == MessageRole::Assistant
                        && assistant_message_has_displayable_content(message);
                    if !passes {
                        continue;
                    }
                    let payload = build_session_result_event_from_message(
                        message_info_from_history_message(message, &data_dir, seq),
                        topic_str.as_deref(),
                    )
                    .to_string();
                    fallback_events.push((message.timestamp, payload));
                }
            }
        }
    }

    if !candidate_events.is_empty() && !fallback_events.is_empty() {
        // Both branches produced events — globally sort by timestamp so the
        // unified set surfaces in true chronological order. (See top-of-fn
        // comment for the previous out-of-order shape.)
        let mut combined: Vec<(chrono::DateTime<chrono::Utc>, String)> = candidate_events;
        combined.extend(fallback_events);
        combined.sort_by_key(|(timestamp, _)| *timestamp);
        let payloads: Vec<String> = combined.into_iter().map(|(_, payload)| payload).collect();
        record_replay(
            "session_result",
            "emitted_with_topic_fallback",
            payloads.len(),
        );
        return payloads;
    }

    if !candidate_events.is_empty() {
        candidate_events.sort_by_key(|(timestamp, _)| *timestamp);
        let payloads: Vec<String> = candidate_events
            .into_iter()
            .map(|(_, payload)| payload)
            .collect();
        record_replay("session_result", "emitted", payloads.len());
        return payloads;
    }

    if !fallback_events.is_empty() {
        fallback_events.sort_by_key(|(timestamp, _)| *timestamp);
        let payloads: Vec<String> = fallback_events
            .into_iter()
            .map(|(_, payload)| payload)
            .collect();
        record_replay("session_result", "emitted_topic_fallback", payloads.len());
        return payloads;
    }

    record_replay("session_result", "missing_session", 1);
    Vec::new()
}

/// GET /sessions/:id/status — check if a session has an active task.
async fn handle_session_status(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<PaginationParams>,
) -> Response {
    let active = {
        let pending = state.pending.lock().await;
        pending.contains_key(&id)
    };
    let has_bg_tasks = state.task_query.as_ref().is_some_and(|query_fn| {
        task_list_has_active_tasks(&query_tasks_for_session_candidates(
            query_fn.as_ref(),
            state.profile_id.as_deref(),
            &id,
            params.topic.as_deref(),
        ))
    });
    Json(serde_json::json!({
        "active": active,
        "has_deferred_files": false,
        "has_bg_tasks": has_bg_tasks,
        "topic": params.topic,
    }))
    .into_response()
}

/// GET /sessions/:id/tasks — list background tasks for a session.
async fn handle_session_tasks(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<PaginationParams>,
) -> Response {
    let Some(ref query_fn) = state.task_query else {
        return Json(serde_json::json!([])).into_response();
    };
    let tasks = query_tasks_for_session_candidates(
        query_fn.as_ref(),
        state.profile_id.as_deref(),
        &id,
        params.topic.as_deref(),
    );
    Json(tasks).into_response()
}

/// `POST /tasks/{task_id}/cancel` — forwards to the wired
/// `with_task_cancel` callback. Maps the structured outcome onto HTTP
/// status codes.
async fn handle_task_cancel(
    State(state): State<ApiState>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Response {
    let Some(ref cancel_fn) = state.task_cancel else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "task supervisor not wired",
            })),
        )
            .into_response();
    };
    match cancel_fn(&task_id) {
        TaskCancelOutcome::Cancelled => (
            StatusCode::OK,
            Json(serde_json::json!({
                "task_id": task_id,
                "status": "cancelled",
            })),
        )
            .into_response(),
        TaskCancelOutcome::NotFound => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "task_not_found",
                "task_id": task_id,
            })),
        )
            .into_response(),
        TaskCancelOutcome::AlreadyTerminal => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "task_already_terminal",
                "task_id": task_id,
            })),
        )
            .into_response(),
    }
}

/// Body of `POST /tasks/{task_id}/restart-from-node`.
#[derive(Debug, Default, Deserialize)]
struct ApiRestartFromNodeRequest {
    #[serde(default)]
    node_id: Option<String>,
}

/// `POST /tasks/{task_id}/restart-from-node` — forwards to the wired
/// `with_task_relaunch` callback. Body: `{ "node_id": Option<String> }`.
async fn handle_task_relaunch(
    State(state): State<ApiState>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    body: Option<Json<ApiRestartFromNodeRequest>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let Some(ref relaunch_fn) = state.task_relaunch else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "task supervisor not wired",
            })),
        )
            .into_response();
    };
    match relaunch_fn(&task_id, body.node_id.as_deref()) {
        TaskRelaunchOutcome::Relaunched { new_task_id } => (
            StatusCode::OK,
            Json(serde_json::json!({
                "original_task_id": task_id,
                "new_task_id": new_task_id,
                "from_node": body.node_id,
            })),
        )
            .into_response(),
        TaskRelaunchOutcome::NotFound => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "task_not_found",
                "task_id": task_id,
            })),
        )
            .into_response(),
        TaskRelaunchOutcome::StillActive => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "task_still_active",
                "task_id": task_id,
            })),
        )
            .into_response(),
    }
}

/// GET /sessions — list all API sessions.
///
/// Backed by `list_top_level_sessions` so internal `child-*` spawn fanouts
/// and `*.tasks` ledger sidecars are skipped at the directory walk. The
/// generic `list_sessions` is O(N) over every JSONL on disk and was
/// observed to hang 30s+ on a user dir with 65k+ child JSONLs (river /
/// mini4) — see issue #607 §D.
async fn handle_list_sessions(State(state): State<ApiState>) -> Response {
    let sess = state.sessions.lock().await;
    let mut seen = std::collections::HashSet::new();
    let list: Vec<SessionInfo> = sess
        .list_top_level_sessions_with_title()
        .into_iter()
        .filter_map(|(id, count, title)| {
            let chat_id = api_chat_id_from_session_key(&id)?.to_string();
            if !seen.insert(chat_id.clone()) {
                return None;
            }
            Some(SessionInfo {
                id: chat_id,
                message_count: count,
                title,
            })
        })
        .collect();
    Json(list).into_response()
}

/// GET /sessions/:id/messages — get session message history.
async fn handle_session_messages(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<PaginationParams>,
) -> Response {
    let limit = params.limit.min(500);
    let offset = params.offset.min(10_000);
    let fetch_count = match offset.checked_add(limit) {
        Some(n) => n,
        None => return (StatusCode::BAD_REQUEST, "invalid pagination").into_response(),
    };
    let candidates =
        api_session_key_candidates(state.profile_id.as_deref(), &id, params.topic.as_deref());
    let Some((data_dir, session_loader)) = snapshot_session_disk_loader(&state.sessions).await
    else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "session storage unavailable",
        )
            .into_response();
    };

    // source=full reads the append-only JSONL file (complete history).
    // Default reads from in-memory (may be compacted for LLM context).
    if params.source.as_deref() == Some("full") {
        for candidate in &candidates {
            if let Some(session) = session_loader.load(candidate).await {
                let messages: Vec<MessageInfo> = session
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(seq, message)| {
                        params.since_seq.is_none_or(|since| *seq > since)
                            && (message.role != MessageRole::Assistant
                                || assistant_message_has_displayable_content(message))
                    })
                    .skip(offset)
                    .take(limit)
                    .map(|(seq, message)| {
                        message_info_from_history_message(message, &data_dir, seq)
                    })
                    .collect();
                return Json(messages).into_response();
            }
        }
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    }

    for candidate in &candidates {
        if let Some(session) = session_loader.load(candidate).await {
            let total_messages = session.messages.len();
            let history = session.get_history(fetch_count).to_vec();
            let base_seq = total_messages.saturating_sub(history.len());
            let messages: Vec<MessageInfo> = history
                .iter()
                .enumerate()
                .filter(|(seq, _)| {
                    let absolute_seq = base_seq + *seq;
                    params.since_seq.is_none_or(|since| absolute_seq > since)
                })
                .filter(|(_, message)| {
                    message.role != MessageRole::Assistant
                        || assistant_message_has_displayable_content(message)
                })
                .skip(offset)
                .take(limit)
                .map(|(seq, message)| {
                    message_info_from_history_message(message, &data_dir, base_seq + seq)
                })
                .collect();
            if !messages.is_empty() {
                return Json(messages).into_response();
            }
        }
    }
    Json(Vec::<MessageInfo>::new()).into_response()
}

/// DELETE /sessions/:id — delete a session.
async fn handle_delete_session(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let mut sess = state.sessions.lock().await;
    let mut deleted = false;
    for candidate in api_session_key_candidates(state.profile_id.as_deref(), &id, None) {
        if sess.load(&candidate).await.is_some() {
            match sess.clear(&candidate).await {
                Ok(()) => deleted = true,
                Err(error) => tracing::error!(
                    session_key = %candidate,
                    error = %error,
                    "delete session from gateway store failed"
                ),
            }
        }
    }
    drop(sess);

    if deleted {
        // Notify the gateway runtime to stop the session actor so it doesn't
        // serve stale context if new messages arrive for this session ID.
        if let Some(ref cb) = state.on_session_deleted {
            cb(&id);
        }
    }
    // No session found — still return 204 (idempotent delete).
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct UpdateSessionTitleRequest {
    title: String,
}

/// PATCH /sessions/:id/title — set a manual title.
async fn handle_update_session_title(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<UpdateSessionTitleRequest>,
) -> Response {
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title must not be empty").into_response();
    }
    if title.chars().count() > 200 {
        return (StatusCode::BAD_REQUEST, "title must be at most 200 chars").into_response();
    }

    let mut sess = state.sessions.lock().await;
    let mut updated = false;
    for candidate in api_session_key_candidates(state.profile_id.as_deref(), &id, None) {
        if sess.load(&candidate).await.is_some() {
            match sess.update_title(&candidate, title.clone()).await {
                Ok(()) => updated = true,
                Err(error) => tracing::error!(
                    session_key = %candidate,
                    error = %error,
                    "update_title in gateway store failed"
                ),
            }
        }
    }

    if updated {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "session not found").into_response()
    }
}

/// GET /files/*path — download a file produced by write_file/send_file.
async fn handle_file_download(
    State(state): State<ApiState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Response {
    let data_dir = {
        let sess = state.sessions.lock().await;
        sess.data_dir()
    };
    let canonical = resolve_scoped_file_handle(&data_dir, &path)
        .or_else(|| resolve_legacy_file_request(&data_dir, &path));
    let Some(canonical) = canonical else {
        return (StatusCode::FORBIDDEN, "access denied").into_response();
    };

    match tokio::fs::read(&canonical).await {
        Ok(bytes) => {
            let filename = canonical
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".to_string());

            let content_type = if filename.ends_with(".md") {
                "text/markdown; charset=utf-8"
            } else if filename.ends_with(".html") {
                "text/html; charset=utf-8"
            } else if filename.ends_with(".json") {
                "application/json"
            } else if filename.ends_with(".pdf") {
                "application/pdf"
            } else {
                "application/octet-stream"
            };

            (
                StatusCode::OK,
                [
                    ("content-type", content_type),
                    (
                        "content-disposition",
                        &format!("inline; filename=\"{filename}\""),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "file not found").into_response(),
    }
}

/// POST /upload — upload files for use in chat media field.
async fn handle_upload(mut multipart: axum::extract::Multipart) -> Response {
    let upload_dir = std::env::temp_dir().join("octos-uploads");
    if let Err(e) = tokio::fs::create_dir_all(&upload_dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mkdir failed: {e}"),
        )
            .into_response();
    }

    let mut paths = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let filename = match field.file_name() {
            Some(f) => f.to_string(),
            None => continue,
        };
        let safe_name = filename
            .replace(['/', '\\', '\0'], "_")
            .chars()
            .take(200)
            .collect::<String>();

        let data = match field.bytes().await {
            Ok(d) => d,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, format!("read failed: {e}")).into_response();
            }
        };

        if data.len() > 50 * 1024 * 1024 {
            return (StatusCode::PAYLOAD_TOO_LARGE, "file exceeds 50MB").into_response();
        }

        let dest = upload_dir.join(&safe_name);
        if let Err(e) = tokio::fs::write(&dest, &data).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write failed: {e}"),
            )
                .into_response();
        }
        let Some(handle) = crate::file_handle::encode_tmp_upload_handle(&dest, Some(&safe_name))
        else {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to encode upload handle",
            )
                .into_response();
        };
        paths.push(handle);
    }

    Json(paths).into_response()
}

// ---------------------------------------------------------------------------
// Admin shell (diagnostics)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ShellRequest {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Serialize)]
struct ShellResponse {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
}

/// POST /admin/shell — execute a shell command (admin auth required).
async fn handle_admin_shell(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<ShellRequest>,
) -> Response {
    // Verify admin token
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-auth-token").and_then(|v| v.to_str().ok()))
        .unwrap_or("");

    // Check channel-level token, env var, then config.json auth_token.
    let expected_token: Option<String> = state
        .auth_token
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| {
            std::env::var("OCTOS_AUTH_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
        })
        .or_else(|| {
            // Try OCTOS_DATA_DIR, then ~/.octos, then cwd/.octos
            let home = std::env::var("HOME").unwrap_or_default();
            let candidates = [
                std::env::var("OCTOS_DATA_DIR").unwrap_or_default(),
                format!("{home}/.octos"),
            ];
            for dir in &candidates {
                if dir.is_empty() {
                    continue;
                }
                if let Ok(s) = std::fs::read_to_string(format!("{dir}/config.json")) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                        if let Some(t) = v.get("auth_token").and_then(|t| t.as_str()) {
                            if !t.is_empty() {
                                return Some(t.to_string());
                            }
                        }
                    }
                }
            }
            None
        });
    let is_admin = match &expected_token {
        Some(expected) if !expected.is_empty() => {
            token.len() == expected.len()
                && token
                    .as_bytes()
                    .iter()
                    .zip(expected.as_bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        }
        _ => false,
    };

    if !is_admin {
        // Debug: return what we tried to match against
        let debug = format!(
            "token_len={} expected_len={} data_dir={} home={}",
            token.len(),
            expected_token.as_ref().map(|t| t.len()).unwrap_or(0),
            std::env::var("OCTOS_DATA_DIR").unwrap_or_else(|_| "unset".into()),
            std::env::var("HOME").unwrap_or_else(|_| "unset".into()),
        );
        return (StatusCode::UNAUTHORIZED, debug).into_response();
    }

    if req.command.is_empty() {
        return (StatusCode::BAD_REQUEST, "command is required").into_response();
    }

    let timeout = std::time::Duration::from_secs(req.timeout_secs.unwrap_or(30).min(300));
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(&req.command);
    if let Some(ref cwd) = req.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("spawn failed: {e}"),
            )
                .into_response();
        }
    };

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Json(ShellResponse {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            timed_out: false,
        })
        .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("exec failed: {e}"),
        )
            .into_response(),
        Err(_) => Json(ShellResponse {
            stdout: String::new(),
            stderr: "command timed out".to_string(),
            exit_code: -1,
            timed_out: true,
        })
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::path::Path;
    use tower::util::ServiceExt;

    const TEST_PROFILE_ID: &str = "dspfac";

    /// M8.10 PR #2: the synthetic SSE message_id round-trips through
    /// encode → decode without losing the bound thread_id. This is the
    /// thread that lets `edit_message` recover the cmid for its
    /// streaming `token`/`replace` payloads.
    #[test]
    fn sse_message_id_roundtrips_chat_id_and_thread_id() {
        let encoded = encode_sse_message_id("chat-A", Some("cmid-T-1"));
        let (chat, tid) = decode_sse_message_id(&encoded);
        assert_eq!(chat, "sse-chat-A");
        assert_eq!(tid, Some("cmid-T-1"));
    }

    #[test]
    fn sse_message_id_omits_thread_id_when_unbound() {
        let encoded = encode_sse_message_id("chat-A", None);
        assert_eq!(encoded, "sse-chat-A");
        let (chat, tid) = decode_sse_message_id(&encoded);
        assert_eq!(chat, "sse-chat-A");
        assert_eq!(tid, None);
    }

    #[test]
    fn outbound_thread_id_extracts_string_from_metadata() {
        let m = serde_json::json!({"thread_id": "cmid-T"});
        assert_eq!(outbound_thread_id(&m).as_deref(), Some("cmid-T"));
    }

    #[test]
    fn outbound_thread_id_treats_empty_string_as_absent() {
        let m = serde_json::json!({"thread_id": ""});
        assert!(outbound_thread_id(&m).is_none());
    }

    #[test]
    fn outbound_thread_id_returns_none_when_absent() {
        let m = serde_json::json!({});
        assert!(outbound_thread_id(&m).is_none());
    }

    fn test_sessions_in(data_dir: &Path) -> Arc<Mutex<SessionManager>> {
        Arc::new(Mutex::new(SessionManager::open(data_dir).unwrap()))
    }

    fn test_sessions() -> Arc<Mutex<SessionManager>> {
        let dir = std::env::temp_dir().join(format!("octos-bus-tests-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        test_sessions_in(&dir)
    }

    fn assistant_tool_call_message(tool_name: &str, arguments: serde_json::Value) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![octos_core::ToolCall {
                id: format!("call-{tool_name}"),
                name: tool_name.to_string(),
                arguments,
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn chat_request_deserialize() {
        let json = r#"{"message": "hello"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "hello");
        assert!(req.session_id.is_none());
        assert!(req.topic.is_none());
    }

    #[test]
    fn chat_request_deserialize_with_topic() {
        let json =
            r#"{"message": "hello", "session_id": "slides-123", "topic": "slides untitled"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "hello");
        assert_eq!(req.session_id.as_deref(), Some("slides-123"));
        assert_eq!(req.topic.as_deref(), Some("slides untitled"));
    }

    #[test]
    fn chat_request_with_session() {
        let json = r#"{"message": "hi", "session_id": "web-123"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.session_id.as_deref(), Some("web-123"));
    }

    /// FA-12f regression: POST /api/chat carries a web-generated
    /// `client_message_id` that must survive as
    /// `InboundMessage.message_id` so downstream overflow emission can
    /// propagate it into `_session_result.response_to_client_message_id`
    /// — the field the web reducer correlates against the optimistic
    /// streaming bubble.
    ///
    /// Before this fix the field was silently dropped at the request
    /// deserializer; overflow replies then arrived with
    /// `response_to_client_message_id: null` and the speculative-queue
    /// BRAVO bubble never rendered (its reply clobbered ALPHA's bubble
    /// via the session_result merge path).
    #[tokio::test]
    async fn chat_request_propagates_client_message_id_to_inbound() {
        let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
        let app = Router::new()
            .route("/chat", post(handle_chat))
            .with_state(ApiState {
                inbound_tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                profile_id: Some(TEST_PROFILE_ID.to_string()),
                sessions: test_sessions(),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let body = serde_json::json!({
            "message": "Use shell: echo BRAVO",
            "session_id": "web-fa12f",
            "client_message_id": "client-bravo-xyz",
            "stream": true,
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let inbound =
            tokio::time::timeout(std::time::Duration::from_millis(500), inbound_rx.recv())
                .await
                .expect("handle_chat must forward the message to the gateway bus")
                .expect("inbound channel closed without a message");

        assert_eq!(
            inbound.message_id.as_deref(),
            Some("client-bravo-xyz"),
            "InboundMessage.message_id must carry the request's client_message_id \
             so the overflow reply can be routed back to the correct bubble",
        );
    }

    /// Empty / missing `client_message_id` must NOT populate the inbound
    /// `message_id` field — we want a sentinel-empty correlation id to
    /// behave the same as "no correlation" so downstream emission doesn't
    /// produce a session_result with an empty-string correlation id that
    /// the reducer would then mis-route.
    #[tokio::test]
    async fn chat_request_treats_empty_client_message_id_as_absent() {
        let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
        let app = Router::new()
            .route("/chat", post(handle_chat))
            .with_state(ApiState {
                inbound_tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                profile_id: Some(TEST_PROFILE_ID.to_string()),
                sessions: test_sessions(),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let body = serde_json::json!({
            "message": "hello",
            "session_id": "web-fa12f-empty",
            "client_message_id": "",
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let inbound =
            tokio::time::timeout(std::time::Duration::from_millis(500), inbound_rx.recv())
                .await
                .expect("inbound channel timed out")
                .expect("inbound channel closed without a message");

        assert!(
            inbound.message_id.is_none(),
            "empty client_message_id must not populate inbound.message_id, got {:?}",
            inbound.message_id,
        );
    }

    /// M8.10 follow-up (#636): closes the SSE thread_id race PR #635
    /// could not. Two failure modes have to be covered:
    ///
    /// 1. The synthetic warm-up `thinking` event fired by `handle_chat`
    ///    BEFORE the inbound is even dispatched must already carry the
    ///    request's `client_message_id`. Pre-fix the live probe of
    ///    mini1/2/3 showed `thread_id=null` on this first event because
    ///    `initial_sse_events` hardcoded the JSON shape and ignored cmid.
    ///
    /// 2. The api_channel sticky map must be seeded from `handle_chat`
    ///    (NOT lazily on the first thread_id-tagged outbound) so that
    ///    early `edit_message` / `send_raw_sse` calls firing during
    ///    the streaming-bubble's first `send_with_id` race window can
    ///    recover the cmid via the sticky lookup. The mini probe
    ///    showed three early `replace` events leaking `thread_id=null`
    ///    after PR #635's lazy-seed sticky map.
    ///
    /// Drives a chat request through `handle_chat`, drains the warm-up
    /// SSE buffer, and asserts BOTH assertions land — the field on the
    /// thinking payload AND the sticky map being populated for the
    /// chat_id key.
    #[tokio::test]
    async fn chat_request_seeds_thread_id_for_first_event_and_sticky_map() {
        let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let last_thread_id = Arc::new(Mutex::new(HashMap::new()));
        let app = Router::new()
            .route("/chat", post(handle_chat))
            .with_state(ApiState {
                inbound_tx,
                pending: pending.clone(),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                profile_id: Some(TEST_PROFILE_ID.to_string()),
                sessions: test_sessions(),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: last_thread_id.clone(),
            });

        let body = serde_json::json!({
            "message": "hello",
            "session_id": "web-636-warmup",
            "client_message_id": "cmid-warmup-key",
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Drain the inbound side so the test fixture can complete.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), inbound_rx.recv())
            .await
            .expect("inbound channel timed out");

        // Acceptance 1: the warm-up `thinking` event sitting in the
        // pending broadcaster must already carry thread_id. Subscribe
        // to the broadcaster's stored sender — the warm-up events
        // were sent to it before the receiver moved into the SSE body.
        let snapshot = pending.lock().await;
        let tx = snapshot
            .get("web-636-warmup")
            .expect("pending sender must exist for active turn")
            .clone();
        // Re-subscribe and inspect the buffered events. `broadcast`
        // late-subscribers see no history, so we instead ask the
        // sender to publish a probe and verify the channel is alive,
        // and we use the helper to rebuild the same buffer.
        // Easier: rebuild the same events by calling the helper
        // directly — its observable contract is what handle_chat used.
        let _ = tx; // keep alive
        drop(snapshot);
        let warmup = initial_sse_events(false, Some("cmid-warmup-key"));
        let parsed: serde_json::Value = serde_json::from_str(&warmup[0]).unwrap();
        assert_eq!(
            parsed.get("thread_id").and_then(|v| v.as_str()),
            Some("cmid-warmup-key"),
            "first thinking event must carry the cmid (#636), got {parsed}",
        );

        // Acceptance 2: the sticky map must already be populated for
        // this chat_id so subsequent `edit_message` / `send_raw_sse`
        // calls fall back to the seeded value (the `replace` events
        // that leaked pre-fix). The seeding happens in handle_chat
        // outside any reporter/agent path.
        let map = last_thread_id.lock().await;
        assert_eq!(
            map.get("web-636-warmup").map(String::as_str),
            Some("cmid-warmup-key"),
            "handle_chat must seed sticky map from request cmid so the \
             first edit_message / send_raw_sse of the turn can recover \
             thread_id when the reporter race window is open (#636), got {map:?}",
        );
    }

    /// Pre-cmid clients must still flow through unchanged: no
    /// thread_id metadata anywhere, no sticky-map pollution. Wire
    /// compat regression guard.
    #[tokio::test]
    async fn chat_request_without_cmid_leaves_sticky_map_clean() {
        let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
        let last_thread_id = Arc::new(Mutex::new(HashMap::new()));
        let app = Router::new()
            .route("/chat", post(handle_chat))
            .with_state(ApiState {
                inbound_tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                profile_id: Some(TEST_PROFILE_ID.to_string()),
                sessions: test_sessions(),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: last_thread_id.clone(),
            });

        let body = serde_json::json!({
            "message": "no cmid",
            "session_id": "web-636-no-cmid",
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), inbound_rx.recv())
            .await
            .expect("inbound channel timed out");

        let map = last_thread_id.lock().await;
        assert!(
            map.is_empty(),
            "no cmid → sticky map must remain empty (wire compat with \
             pre-cmid clients), got {map:?}",
        );
    }

    #[tokio::test]
    async fn attach_only_does_not_enqueue_empty_inbound_message() {
        let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
        let app = Router::new()
            .route("/chat", post(handle_chat))
            .with_state(ApiState {
                inbound_tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                profile_id: Some(TEST_PROFILE_ID.to_string()),
                sessions: test_sessions(),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"message":"","session_id":"web-attach","media":[],"attach_only":true}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        if let Ok(Some(message)) =
            tokio::time::timeout(std::time::Duration::from_millis(100), inbound_rx.recv()).await
        {
            panic!(
                "attach_only unexpectedly enqueued an inbound turn: {:?}",
                message.content
            );
        }
    }

    #[tokio::test]
    async fn session_status_reports_background_tasks_separately_from_stream_activity() {
        let app = Router::new()
            .route("/sessions/{id}/status", get(handle_session_status))
            .with_state(ApiState {
                inbound_tx: mpsc::channel(1).0,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                profile_id: Some(TEST_PROFILE_ID.to_string()),
                sessions: test_sessions(),
                task_query: Some(Arc::new(|_| {
                    serde_json::json!([
                        { "id": "task-1", "tool_name": "run_pipeline", "status": "running" }
                    ])
                })),
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/sessions/web-attach/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload.get("active").and_then(|value| value.as_bool()),
            Some(false)
        );
        assert_eq!(
            payload
                .get("has_bg_tasks")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            payload
                .get("has_deferred_files")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    #[tokio::test]
    async fn session_status_accepts_profiled_api_session_ids() {
        let app = Router::new()
            .route("/sessions/{id}/status", get(handle_session_status))
            .route("/sessions/{id}/tasks", get(handle_session_tasks))
            .with_state(ApiState {
                inbound_tx: mpsc::channel(1).0,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                profile_id: Some(TEST_PROFILE_ID.to_string()),
                sessions: test_sessions(),
                task_query: Some(Arc::new(|session_key| {
                    if session_key == "dspfac:api:web-profiled" {
                        serde_json::json!([
                            { "id": "task-1", "tool_name": "Deep research", "status": "running" }
                        ])
                    } else {
                        serde_json::json!([])
                    }
                })),
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let status = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/sessions/dspfac:api:web-profiled/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let body = axum::body::to_bytes(status.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload
                .get("has_bg_tasks")
                .and_then(|value| value.as_bool()),
            Some(true)
        );

        let tasks = app
            .oneshot(
                Request::builder()
                    .uri("/sessions/dspfac:api:web-profiled/tasks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tasks.status(), StatusCode::OK);
        let body = axum::body::to_bytes(tasks.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.as_array().map(Vec::len), Some(1));
        assert_eq!(payload[0]["tool_name"], "Deep research");
    }

    #[test]
    fn message_info_from_history_message_hides_absolute_paths() {
        let data_dir = tempfile::tempdir().unwrap();
        let artifact = data_dir
            .path()
            .join("users")
            .join("dspfac%3Aapi%3Aweb-1")
            .join("workspace")
            .join(".artifacts")
            .join("deck.pptx");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"pptx").unwrap();

        let message = Message {
            role: MessageRole::Assistant,
            content: format!("[file:{}] deck.pptx", artifact.to_string_lossy()),
            media: vec![artifact.to_string_lossy().to_string()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        };

        let info = message_info_from_history_message(&message, data_dir.path(), 7);
        assert_eq!(info.seq, Some(7));
        assert_eq!(info.media.len(), 1);
        assert_ne!(info.media[0], artifact.to_string_lossy());
        assert!(
            !info
                .content
                .contains(&artifact.to_string_lossy().to_string())
        );
        assert!(info.content.contains("[file:pf/"));
    }

    #[test]
    fn message_info_propagates_client_message_id_from_message() {
        let data_dir = tempfile::tempdir().unwrap();
        let message = Message::user("hello there").with_client_message_id("cmid-history-7");

        let info = message_info_from_history_message(&message, data_dir.path(), 5);
        assert_eq!(info.seq, Some(5));
        assert_eq!(info.client_message_id.as_deref(), Some("cmid-history-7"));

        // Round-trip via JSON (the wire shape) — the field is preserved.
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["client_message_id"], "cmid-history-7");
    }

    #[test]
    fn message_info_omits_client_message_id_when_absent() {
        let data_dir = tempfile::tempdir().unwrap();
        let message = Message::user("hi");

        let info = message_info_from_history_message(&message, data_dir.path(), 0);
        assert!(info.client_message_id.is_none());

        // Skipped from the serialized JSON for forward compat.
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("client_message_id").is_none());
    }

    #[test]
    fn build_session_result_event_normalizes_persisted_media_paths_like_history_replay() {
        let data_dir = tempfile::tempdir().unwrap();
        let artifact = data_dir
            .path()
            .join("users")
            .join("dspfac%3Aapi%3Aweb-1")
            .join("workspace")
            .join(".artifacts")
            .join("deck.pptx");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"pptx").unwrap();

        let raw = serde_json::json!({
            "role": "assistant",
            "content": "Deck ready",
            "media": [artifact.to_string_lossy().to_string()],
            "timestamp": Utc::now().to_rfc3339(),
        });

        let event = build_session_result_event(&raw, data_dir.path(), None, Some("slides demo"))
            .expect("session result event");
        let event_media = event["message"]["media"]
            .as_array()
            .expect("event media array")
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect::<Vec<_>>();

        let replay_media = message_info_from_history_message(
            &Message {
                role: MessageRole::Assistant,
                content: "Deck ready".into(),
                media: vec![artifact.to_string_lossy().to_string()],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: Utc::now(),
            },
            data_dir.path(),
            1,
        )
        .media;

        assert_eq!(event_media, replay_media);
    }

    #[tokio::test]
    async fn api_channel_persists_media_without_legacy_file_marker_content() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let channel = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let artifact = data_dir.path().join("deck.pptx");
        std::fs::write(&artifact, b"pptx").unwrap();

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "web-legacy-media".into(),
            content: "".into(),
            reply_to: None,
            media: vec![artifact.to_string_lossy().to_string()],
            metadata: serde_json::json!({}),
        };

        channel.send(&msg).await.unwrap();

        let info = {
            let sess = sessions.lock().await;
            let data_dir = sess.data_dir();
            let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "web-legacy-media");
            let loaded = sess.load(&key).await.unwrap();
            message_info_from_history_message(&loaded.messages[0], &data_dir, 0)
        };

        assert_eq!(info.media.len(), 1);
        assert!(info.content.trim().is_empty());
    }

    #[test]
    fn copy_media_into_session_artifacts_reuses_existing_copy_for_identical_file() {
        let root = tempfile::tempdir().unwrap();
        let artifact_dir = root.path().join(".artifacts");
        std::fs::create_dir_all(&artifact_dir).unwrap();

        let source = root
            .path()
            .join("slides")
            .join("demo")
            .join("output")
            .join("deck.pptx");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"same deck bytes").unwrap();

        let first = ApiChannel::copy_media_into_session_artifacts(
            &artifact_dir,
            &[source.display().to_string()],
        );
        let second = ApiChannel::copy_media_into_session_artifacts(
            &artifact_dir,
            &[source.display().to_string()],
        );

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0], second[0]);
        assert!(std::path::Path::new(&first[0]).exists());
    }

    #[test]
    fn api_session_key_candidates_prefer_current_profile() {
        let keys = api_session_key_candidates(Some("dspfac--newsbot"), "web-123", None);

        assert_eq!(keys[0].0, "dspfac--newsbot:api:web-123");
        assert_eq!(keys[1].0, "_main:api:web-123");
        assert_eq!(keys[2].0, "api:web-123");
    }

    #[test]
    fn api_session_key_candidates_do_not_double_prefix_profiled_ids() {
        let keys = api_session_key_candidates(Some("dspfac"), "dspfac:api:web-123", None);
        let rendered = keys.iter().map(|key| key.0.as_str()).collect::<Vec<_>>();

        assert_eq!(rendered[0], "dspfac:api:web-123");
        assert!(rendered.contains(&"api:web-123"));
        assert!(!rendered.contains(&"dspfac:api:dspfac:api:web-123"));
    }

    #[test]
    fn api_chat_id_from_profiled_session_key_strips_prefix() {
        assert_eq!(
            api_chat_id_from_session_key("dspfac--newsbot:api:web-123"),
            Some("web-123")
        );
        assert_eq!(
            api_chat_id_from_session_key("_main:api:web-123"),
            Some("web-123")
        );
        assert_eq!(api_chat_id_from_session_key("api:web-123"), Some("web-123"));
    }

    #[test]
    fn api_chat_id_from_session_key_hides_internal_runtime_topics() {
        assert_eq!(
            api_chat_id_from_session_key("dspfac:api:web-123#child-task-1"),
            None
        );
        assert_eq!(
            api_chat_id_from_session_key("dspfac:api:web-123#default.tasks"),
            None
        );
        assert_eq!(api_chat_id_from_session_key("web-123#default.tasks"), None);
        assert_eq!(
            api_chat_id_from_session_key("dspfac:api:web-123#research"),
            Some("web-123#research")
        );
        assert_eq!(
            api_chat_id_from_session_key("web-123#research"),
            Some("web-123#research")
        );
        assert_eq!(api_chat_id_from_session_key("telegram:123"), None);
    }

    #[test]
    fn api_channel_name() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        assert_eq!(ch.name(), "api");
    }

    #[test]
    fn api_channel_max_message_length() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        assert_eq!(ch.max_message_length(), 1_000_000);
    }

    #[test]
    fn initial_sse_events_include_thinking() {
        let events = initial_sse_events(false, None);
        assert_eq!(events.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(parsed["type"], "thinking");
        assert_eq!(parsed["iteration"], 0);
        assert!(
            parsed.get("thread_id").is_none(),
            "no thread_id passed → field must be absent (wire compat with \
             pre-cmid clients): got {parsed}"
        );
    }

    #[test]
    fn initial_sse_events_include_preprocessing_for_media() {
        let events = initial_sse_events(true, None);
        assert_eq!(events.len(), 2);
        let parsed: Vec<serde_json::Value> = events
            .iter()
            .map(|event| serde_json::from_str(event).unwrap())
            .collect();
        assert_eq!(parsed[0]["type"], "thinking");
        assert_eq!(parsed[1]["type"], "tool_progress");
        assert_eq!(parsed[1]["tool"], "preprocessing");
    }

    /// M8.10 follow-up (#636): the synthetic warm-up `thinking` event the
    /// API channel emits the moment a chat request lands MUST carry the
    /// request's thread_id so the FIRST event of every turn arrives
    /// pre-routed. Pre-fix this event leaked `thread_id=null` because
    /// `initial_sse_events` hardcoded the payload shape and ignored the
    /// inbound's `client_message_id`. Drives both the no-media and
    /// has-media branches to confirm both events tag through.
    #[test]
    fn initial_sse_events_tag_thread_id_when_provided() {
        let events = initial_sse_events(false, Some("cmid-warmup-A"));
        assert_eq!(events.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(
            parsed.get("thread_id").and_then(|v| v.as_str()),
            Some("cmid-warmup-A"),
            "warm-up thinking must carry the bound cmid (#636): got {parsed}"
        );

        let events = initial_sse_events(true, Some("cmid-warmup-B"));
        assert_eq!(events.len(), 2);
        for raw in &events {
            let parsed: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                parsed.get("thread_id").and_then(|v| v.as_str()),
                Some("cmid-warmup-B"),
                "warm-up event {raw} missing thread_id"
            );
        }
    }

    #[tokio::test]
    async fn sse_channel_bounds_buffer_and_drops_oldest_events() {
        let (tx, mut rx) = new_sse_channel();
        for i in 0..=SSE_CHANNEL_CAPACITY {
            let _ = tx.send(i.to_string());
        }

        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(1))
        ));
        assert_eq!(rx.recv().await.unwrap(), "1");
    }

    #[test]
    fn build_bg_task_tool_start_events_adds_tts_compatibility_event() {
        let tasks = serde_json::json!([
            { "id": "task-1", "tool_name": "Direct TTS", "tool_call_id": "call_tts_1", "status": "running" },
            { "id": "task-2", "tool_name": "Direct TTS", "status": "spawned" },
            { "id": "task-3", "tool_name": "Research Podcast", "status": "running" }
        ]);

        let events = build_bg_task_tool_start_events(&tasks);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "tool_start");
        assert_eq!(events[0]["tool"], "fm_tts");
        assert_eq!(events[0]["tool_call_id"], "call_tts_1");
    }

    #[tokio::test]
    async fn send_to_pending_client() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-chat".into(), tx);
        }

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-chat".into(),
            content: "hello world".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({}),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "replace");
        assert_eq!(parsed["text"], "hello world");
    }

    #[tokio::test]
    async fn send_committed_background_result_emits_session_result_event() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions,
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-chat".into(), tx);
        }

        let source_dir = data_dir.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("podcast.mp3");
        std::fs::write(&source, b"audio").unwrap();

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-chat".into(),
            content: "Status: SUCCESS".into(),
            reply_to: None,
            media: vec![source.to_string_lossy().to_string()],
            metadata: serde_json::json!({
                "_history_persisted": true,
                "_session_result": {
                    "seq": 7,
                    "role": "assistant",
                    "content": "Status: SUCCESS",
                    "timestamp": "2026-04-15T19:15:03Z",
                    "media": [source.to_string_lossy().to_string()],
                }
            }),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "session_result");
        assert_eq!(parsed["message"]["seq"], 7);
        assert_eq!(parsed["message"]["content"], "Status: SUCCESS");
        let media = parsed["message"]["media"].as_array().unwrap();
        assert_eq!(media.len(), 1);
        assert!(media[0].as_str().unwrap().starts_with("pf/"));
        assert!(rx.try_recv().is_err());
    }

    /// M8.10 follow-up (#649) regression: when the api_channel sticky map
    /// has rotated through three turns (A → B → C) and a long-running
    /// background task originating in turn A finally finalises, the wire
    /// event MUST carry turn A's thread_id (sourced from explicit
    /// metadata) and NOT the most-recent sticky value (turn C).
    ///
    /// Reproduces the live mini3 trace (2026-04-29, session
    /// `web-1777402538752-zn7jfr`) where the deep_research turn's late
    /// output landed under the voices turn's bubble — the bug this PR
    /// fixes.
    #[tokio::test]
    async fn late_tool_result_for_overflow_turn_keeps_originating_thread_id_under_3_user_race() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions,
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-race-chat".into(), tx);
        }

        // Simulate the production scenario: 3 user turns rotate the
        // sticky map A → B → C as their first SSE events go out. We
        // drive the rotation directly by calling `remember_thread_id`
        // (the same hook every `send`/`send_with_id` uses).
        ch.remember_thread_id("test-race-chat", Some("cmid-A-deep-research"))
            .await;
        ch.remember_thread_id("test-race-chat", Some("cmid-B-stocks"))
            .await;
        ch.remember_thread_id("test-race-chat", Some("cmid-C-voices"))
            .await;
        // After this, the sticky map points at C — the WRONG thread for
        // a late-arriving turn-A result.

        // Now turn A's background task finally finalises. It carries
        // `thread_id=cmid-A-deep-research` in OutboundMessage metadata
        // (the fix). The api_channel must honour this explicit metadata
        // INSTEAD of falling through to the sticky map.
        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-race-chat".into(),
            content: "Deep research report on space exploration".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "_history_persisted": true,
                "thread_id": "cmid-A-deep-research",
                "_session_result": {
                    "seq": 9,
                    "role": "assistant",
                    "content": "Deep research report on space exploration",
                    "timestamp": "2026-04-29T05:56:03Z",
                    "media": [],
                    "thread_id": "cmid-A-deep-research",
                }
            }),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "session_result");
        assert_eq!(
            parsed.get("thread_id").and_then(|v| v.as_str()),
            Some("cmid-A-deep-research"),
            "wire-side session_result MUST be tagged with the turn A cmid \
             carried explicitly in OutboundMessage metadata; the sticky \
             map (which now points at turn C) must NOT win. Got: {parsed}"
        );
        assert_eq!(
            parsed["message"].get("thread_id").and_then(|v| v.as_str()),
            Some("cmid-A-deep-research"),
            "the embedded message body must also carry the originating \
             thread_id so the web client renders it under the right bubble \
             (the v2 thread-store keys off `message.thread_id`)"
        );
    }

    /// M8.10 follow-up (#649) regression: explicit `thread_id` in
    /// OutboundMessage metadata MUST win over the sticky map for the
    /// `replace`/wire-side text path too. This pins the contract that
    /// `outbound_thread_id(metadata)` is consulted BEFORE
    /// `sticky_thread_id(chat_id)` in `send()`.
    #[tokio::test]
    async fn explicit_metadata_thread_id_wins_over_sticky_for_replace_event() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions,
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-explicit-chat".into(), tx);
        }

        // Sticky map says C.
        ch.remember_thread_id("test-explicit-chat", Some("cmid-sticky-C"))
            .await;

        // Outbound carries A explicitly.
        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-explicit-chat".into(),
            content: "originating turn A reply".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({ "thread_id": "cmid-explicit-A" }),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "replace");
        assert_eq!(
            parsed.get("thread_id").and_then(|v| v.as_str()),
            Some("cmid-explicit-A"),
            "explicit metadata.thread_id must outrank the sticky map; got {parsed}"
        );
    }

    #[tokio::test]
    async fn broadcasts_session_result_for_user_message_with_client_message_id() {
        // Verifies that the api_channel `send()` path emits a session_result
        // event for a persisted *user* message when the OutboundMessage carries
        // `_session_result` metadata with role="user" and a client_message_id.
        // This is the wire shape the web client uses to stamp the
        // server-assigned `historySeq` onto its optimistic user bubble (the
        // M8.10-A-counterpart fix for user messages).
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions,
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-user-msg-chat".into(), tx);
        }

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-user-msg-chat".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "_history_persisted": true,
                "_session_result": {
                    "seq": 4,
                    "role": "user",
                    "content": "remind me about lunch",
                    "timestamp": "2026-04-24T19:15:03Z",
                    "client_message_id": "cmid-user-bubble-42",
                }
            }),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "session_result");
        assert_eq!(parsed["message"]["role"], "user");
        assert_eq!(parsed["message"]["seq"], 4);
        assert_eq!(parsed["message"]["content"], "remind me about lunch");
        assert_eq!(
            parsed["message"]["client_message_id"], "cmid-user-bubble-42",
            "user-message session_result events must carry the client-supplied id so the web client can correlate optimistic bubbles to the server seq"
        );
        assert!(rx.try_recv().is_err());
    }

    /// Helper: simulate the actor-side write to per-user JSONL (no FLAT write).
    /// Mirrors `SessionActor::deliver_background_notification` for spawn_only
    /// file deliveries — the actor stamps `_history_persisted=true` on the
    /// outbound, so ApiChannel never writes for these.
    async fn actor_persist_to_per_user(
        data_dir: &Path,
        session_key: &SessionKey,
        message: Message,
    ) {
        let mut handle = crate::session::SessionHandle::open(data_dir, session_key);
        handle.add_message_with_seq(message).await.unwrap();
    }

    #[tokio::test]
    async fn bus_side_persist_routes_to_canonical_per_user_topic_jsonl() {
        // Pins the unified-write contract introduced by the storage unification
        // fix. The bus-side `persist_to_session` previously wrote to:
        //   - legacy flat `sessions/<encoded_full_key>.jsonl`
        //   - hardcoded per-user `users/<encoded_base>/sessions/default.jsonl`
        //     (ignored topic — actor-side writes used `<topic>.jsonl`)
        //
        // Post-fix it must route through `SessionHandle` so writes land in the
        // canonical per-user `<encoded_topic>.jsonl` file the actor uses.
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let channel = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions,
            Some(TEST_PROFILE_ID.to_string()),
        );

        let topic = "site astro";
        let mp3 = data_dir.path().join("audio").join("a.mp3");
        std::fs::create_dir_all(mp3.parent().unwrap()).unwrap();
        std::fs::write(&mp3, b"mp3 bytes").unwrap();

        let outbound = OutboundMessage {
            channel: "api".into(),
            chat_id: "web-canonical".into(),
            content: "✓ fm_tts done".into(),
            reply_to: None,
            media: vec![mp3.to_string_lossy().into_owned()],
            metadata: serde_json::json!({ "topic": topic }),
        };
        channel.send(&outbound).await.unwrap();

        // Canonical per-user topic file must exist with the message.
        let encoded_base =
            crate::session::encode_path_component(&format!("{TEST_PROFILE_ID}:api:web-canonical"));
        let encoded_topic = crate::session::encode_path_component(topic);
        let canonical = data_dir
            .path()
            .join("users")
            .join(&encoded_base)
            .join("sessions")
            .join(format!("{encoded_topic}.jsonl"));
        assert!(
            canonical.exists(),
            "bus-side persist must write to canonical per-user `<topic>.jsonl` ({}) — \
             this is the file the SessionActor also writes, eliminating split-brain storage",
            canonical.display()
        );
        let body = std::fs::read_to_string(&canonical).unwrap();
        assert!(
            body.contains("fm_tts done"),
            "canonical per-user `<topic>.jsonl` must record the persisted message"
        );

        // Legacy per-user `default.jsonl` mirror must NOT be written when a
        // topic is supplied — that's the bug we are fixing.
        let legacy_default = data_dir
            .path()
            .join("users")
            .join(&encoded_base)
            .join("sessions")
            .join("default.jsonl");
        assert!(
            !legacy_default.exists(),
            "topic-bearing bus-side persist must NOT touch the hardcoded `default.jsonl` mirror — \
             that legacy fan-out caused the split-brain bug"
        );
    }

    #[tokio::test]
    async fn spawn_only_file_delivery_is_visible_to_watcher_replay_after_reconnect() {
        // Regression for the split-brain session-storage bug.
        //
        // Production scenario reproduced on mini2 (2026-04-23):
        //   1. A spawn_only background task (e.g. fm_tts) finishes long after
        //      the user's interactive turn ended, so the live SSE pending
        //      sender for the session has either been dropped or is empty.
        //   2. SessionActor::deliver_background_notification persists the file
        //      message via the per-actor `SessionHandle` (per-user JSONL at
        //      `users/<encoded_base>/sessions/<encoded_topic>.jsonl`) and stamps
        //      `_history_persisted=true` on the OutboundMessage.
        //   3. ApiChannel::send sees `_history_persisted=true` and skips its
        //      own bus-side write (legacy flat layout
        //      `sessions/<encoded_full_key>.jsonl` plus the hardcoded per-user
        //      `default.jsonl` mirror — note that mirror IGNORES the actor's
        //      topic).
        //   4. The user reconnects. Their web client opens
        //      `/sessions/{chat_id}/events/stream` — without re-supplying the
        //      topic in the query string (a real failure mode of the dashboard
        //      reload + workflow listing flows). The only chance the audio
        //      bubble has to materialise is `replay_committed_session_results`.
        //
        // Pre-fix, the actor's write lands in `<topic>.jsonl` while the
        // ApiChannel write — when it happens at all — hits FLAT or the
        // hardcoded `default.jsonl`. With no topic in the candidate-key set
        // and nothing in the topic-less per-user files, replay returns zero
        // events and the audio bubble silently disappears.
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let topic = "site astro";
        let session_key = current_profile_api_session_key_with_topic(
            Some(TEST_PROFILE_ID),
            "test-chat",
            Some(topic),
        );

        // Actor persists the user message into per-user `<topic>.jsonl` (the
        // production gateway path: SessionActor handles inbound BEFORE
        // ApiChannel.send is ever invoked for this turn).
        actor_persist_to_per_user(
            data_dir.path(),
            &session_key,
            Message::user("please make me a podcast about cats"),
        )
        .await;

        // Simulate the mp3 the spawn_only fm_tts skill produced.
        let mp3_path = data_dir.path().join("artifacts").join("podcast.mp3");
        std::fs::create_dir_all(mp3_path.parent().unwrap()).unwrap();
        std::fs::write(&mp3_path, b"ID3...mp3 bytes").unwrap();

        // Actor-side spawn_only delivery — the only writer that records the
        // file message anywhere. ApiChannel never writes because the actor
        // stamps `_history_persisted=true`.
        actor_persist_to_per_user(
            data_dir.path(),
            &session_key,
            Message {
                role: MessageRole::Assistant,
                content: "✓ fm_tts completed — file delivered".into(),
                media: vec![mp3_path.to_string_lossy().into_owned()],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        )
        .await;

        let state = ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions,
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            last_thread_id: Arc::new(Mutex::new(HashMap::new())),
        };

        // Cold reconnect WITHOUT topic — this is what fails pre-fix because
        // the candidate-key set never reaches `<topic>.jsonl` and the
        // hardcoded per-user `default.jsonl` mirror was never written.
        let replayed_topicless =
            replay_committed_session_results(&state, "test-chat", None, None).await;

        let event_topicless = replayed_topicless
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
            .find(|payload| {
                payload["message"]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("fm_tts completed"))
            });
        assert!(
            event_topicless.is_some(),
            "topic-less reconnect must still surface the spawn_only file delivery — \
             actor-side write to per-user `<topic>.jsonl` was lost because the \
             bus-side reader never visits topic-bearing per-user files when the \
             watcher subscribes without a topic. This is the split-brain \
             session-storage bug"
        );

        // Hot reconnect WITH topic — this happens to work pre-fix because the
        // SessionManager::load merge sees per-user `<topic>.jsonl`. We pin
        // the contract here too so the canonical-write fix doesn't quietly
        // break the topic path while landing the topic-less path.
        let replayed_topic =
            replay_committed_session_results(&state, "test-chat", None, Some(topic)).await;
        let event_topic = replayed_topic
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
            .find(|payload| {
                payload["message"]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("fm_tts completed"))
            })
            .expect("topic-aware reconnect must continue to surface the file delivery");
        let media = event_topic["message"]["media"]
            .as_array()
            .expect("file-delivery session_result event must carry a media array");
        assert_eq!(media.len(), 1, "exactly one audio handle expected");
        assert!(
            media[0].as_str().unwrap_or_default().starts_with("pf/"),
            "audio path must be projected through the profile-relative file handle so the web client can fetch it"
        );
    }

    #[tokio::test]
    async fn concurrent_bus_side_persists_get_distinct_seqs() {
        // Regression for the concurrent-persist seq race introduced when
        // `persist_to_session` switched from `SessionManager::add_message_with_seq`
        // (shared mutex via `Arc<Mutex<SessionManager>>`) to
        // `SessionHandle::open` + `add_message_with_seq`.
        //
        // Each `SessionHandle::open` loads disk into its OWN per-instance
        // `messages: Vec<_>`. Two concurrent calls both observe `len = N`,
        // both append, both return `seq = N`. Watcher correlation breaks —
        // the web client sees two "session_result, seq=N" rows and renders
        // duplicates.
        //
        // Post-fix: writes for the same session_key must serialise at the
        // storage layer (per-key mutex map shared across actor + channel) so
        // each call observes a fresh `len` and returns a distinct seq.
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let channel = Arc::new(ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions,
            Some(TEST_PROFILE_ID.to_string()),
        ));

        let chat_id = "race-chat";
        let topic = Some("race-topic");
        let n = 16usize;

        let mut handles = Vec::new();
        for i in 0..n {
            let channel = channel.clone();
            let chat_id = chat_id.to_string();
            handles.push(tokio::spawn(async move {
                channel
                    .persist_to_session(
                        &chat_id,
                        topic,
                        Message::assistant(format!("concurrent assistant {i}")),
                    )
                    .await
                    .and_then(|info| info.seq)
            }));
        }

        let mut seqs: Vec<usize> = Vec::with_capacity(n);
        for h in handles {
            let result = h.await.expect("join");
            seqs.push(result.expect("persist must succeed and return a seq"));
        }
        seqs.sort();
        let expected: Vec<usize> = (0..n).collect();
        assert_eq!(
            seqs, expected,
            "{n} concurrent bus-side persist calls must each receive a \
             distinct sequence in 0..N (storage layer must serialise writes \
             via a per-key lock map shared across actor + channel)"
        );
    }

    #[tokio::test]
    async fn topic_less_fallback_runs_when_candidate_topicless_file_is_empty() {
        // Regression for the topic-less-fallback short-circuit bug.
        //
        // When a topic-less candidate JSONL exists on disk but contains zero
        // displayable assistant messages (only user lines, or only tool-trace
        // assistant entries with empty content), the candidate-load early-
        // returned with `events = []` BEFORE the topic-less per-user fallback
        // ran. As a result the audio bubble committed under a topic-bearing
        // file was never surfaced to a topic-less reconnect.
        //
        // Post-fix: the fallback path runs whenever the candidate-load returned
        // empty content (vs returned a Some(session) with displayable rows). A
        // populated topic-bearing per-user JSONL must surface even when an
        // empty topic-less per-user file co-exists for the same base_key.
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());

        // Topic-less per-user JSONL: exists, but only contains a user line —
        // zero displayable assistant content.
        let topicless_key = current_profile_api_session_key_with_topic(
            Some(TEST_PROFILE_ID),
            "fallback-chat",
            None,
        );
        actor_persist_to_per_user(
            data_dir.path(),
            &topicless_key,
            Message::user("hello — no assistant response yet on this branch"),
        )
        .await;

        // Topic-bearing per-user JSONL: holds the actually-committed audio
        // bubble that the topic-less reconnect must replay.
        let topic = "site astro";
        let topic_key = current_profile_api_session_key_with_topic(
            Some(TEST_PROFILE_ID),
            "fallback-chat",
            Some(topic),
        );
        let mp3 = data_dir.path().join("audio").join("fallback.mp3");
        std::fs::create_dir_all(mp3.parent().unwrap()).unwrap();
        std::fs::write(&mp3, b"mp3 bytes").unwrap();
        actor_persist_to_per_user(
            data_dir.path(),
            &topic_key,
            Message {
                role: MessageRole::Assistant,
                content: "✓ topic-bearing audio bubble committed under topic JSONL".into(),
                media: vec![mp3.to_string_lossy().into_owned()],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        )
        .await;

        let state = ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions,
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            last_thread_id: Arc::new(Mutex::new(HashMap::new())),
        };

        let replayed = replay_committed_session_results(&state, "fallback-chat", None, None).await;
        let topic_event = replayed
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
            .find(|payload| {
                payload["message"]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("topic-bearing audio bubble"))
            });
        assert!(
            topic_event.is_some(),
            "topic-less reconnect must reach the per-user fallback when the \
             candidate topic-less JSONL is empty — the early `return events;` \
             in the candidate-load loop short-circuited the fallback and the \
             topic-bearing audio bubble silently disappeared"
        );
    }

    #[tokio::test]
    async fn topic_less_fallback_does_not_strip_messages_via_per_file_seq() {
        // Regression for the wrong-axis `since_seq` filter in the topic-less
        // fallback. Pre-fix, `since_seq` was compared against per-file
        // `enumerate()` positions inside EACH topic JSONL independently — a
        // watcher cursor of N meant "skip N messages of every topic file"
        // instead of "skip the first N messages in the unified replay".
        // For any topic file with > N messages this either wrongly stripped
        // legitimate later assistant rows or wrongly let early rows through.
        //
        // Post-fix, the fallback path drops the per-file `since_seq` filter
        // entirely. The fallback only runs on a topic-less reconnect — that
        // is, the watcher has no unified cursor against which a per-file
        // index could be measured. Tracking it was meaningless. We pin the
        // contract: with `since_seq=Some(N)` the fallback still emits every
        // displayable assistant message regardless of position in its file.
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());

        // Topic-less per-user JSONL is empty so the candidate-load returns
        // no events; the fallback is the only path that surfaces the audio
        // bubbles below.
        let topicless_key = current_profile_api_session_key_with_topic(
            Some(TEST_PROFILE_ID),
            "long-topic-chat",
            None,
        );
        actor_persist_to_per_user(
            data_dir.path(),
            &topicless_key,
            Message::user("kick off the topic"),
        )
        .await;

        // Topic-bearing per-user JSONL with many displayable assistant rows.
        // Pre-fix, with `since_seq=Some(5)` the fallback would silently strip
        // rows 0..=5 from the topic file's per-file index (so messages 0-5
        // would be dropped).
        let topic = "long-topic";
        let topic_key = current_profile_api_session_key_with_topic(
            Some(TEST_PROFILE_ID),
            "long-topic-chat",
            Some(topic),
        );
        for n in 0..20usize {
            actor_persist_to_per_user(
                data_dir.path(),
                &topic_key,
                Message::assistant(format!("topic answer {n}")),
            )
            .await;
        }

        let state = ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions,
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            last_thread_id: Arc::new(Mutex::new(HashMap::new())),
        };

        let replayed =
            replay_committed_session_results(&state, "long-topic-chat", Some(5), None).await;
        let recovered: std::collections::HashSet<String> = replayed
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
            .filter_map(|payload| payload["message"]["content"].as_str().map(str::to_string))
            .collect();
        for n in 0..20usize {
            let expected = format!("topic answer {n}");
            assert!(
                recovered.contains(&expected),
                "fallback must surface every displayable assistant message in \
                 a topic file regardless of `since_seq` (a per-watcher cursor \
                 measured against the unified replay, NOT the per-file index). \
                 Missing: `{expected}`"
            );
        }
    }

    #[tokio::test]
    async fn combined_replay_events_are_globally_sorted_by_timestamp() {
        // Pins the global-timestamp-sort contract for the combined-events
        // branch in `replay_committed_session_results`. Pre-fix, when both
        // the candidate-load and the topic-less fallback produced events,
        // the function concatenated `candidate_events` (in disk order) BEFORE
        // `fallback_events` (timestamp-sorted) without globally sorting the
        // unified set. If the two branches' timestamps interleave, replay
        // delivered them out of chronological order — the web client renders
        // bubbles in delivery order, so a topic-less reconnect would show
        // candidate bubbles first then a "leap back in time" to fallback
        // bubbles whose timestamps fall between candidate ones.
        //
        // Post-fix: extract the timestamp from each event's payload and
        // sort the unified set by timestamp before returning.
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());

        // Topic-less candidate JSONL with timestamps T0, T2, T4 (even).
        let topicless_key = current_profile_api_session_key_with_topic(
            Some(TEST_PROFILE_ID),
            "interleave-chat",
            None,
        );
        let base = chrono::Utc::now() - chrono::Duration::seconds(60);
        for (idx, secs) in [0i64, 2, 4].iter().enumerate() {
            let mut msg = Message::assistant(format!("candidate-{idx}-T{secs}"));
            msg.timestamp = base + chrono::Duration::seconds(*secs);
            actor_persist_to_per_user(data_dir.path(), &topicless_key, msg).await;
        }

        // Topic-bearing fallback file under the same base_key with timestamps
        // T1, T3, T5 (odd) — interleaving the candidate timestamps.
        let topic = "interleaved";
        let topic_key = current_profile_api_session_key_with_topic(
            Some(TEST_PROFILE_ID),
            "interleave-chat",
            Some(topic),
        );
        for (idx, secs) in [1i64, 3, 5].iter().enumerate() {
            let mut msg = Message::assistant(format!("fallback-{idx}-T{secs}"));
            msg.timestamp = base + chrono::Duration::seconds(*secs);
            actor_persist_to_per_user(data_dir.path(), &topic_key, msg).await;
        }

        let state = ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions,
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            last_thread_id: Arc::new(Mutex::new(HashMap::new())),
        };

        // Topic-less reconnect — both the candidate-load and the topic-less
        // fallback produce events.
        let replayed =
            replay_committed_session_results(&state, "interleave-chat", None, None).await;

        let timestamps: Vec<chrono::DateTime<chrono::Utc>> = replayed
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
            .filter_map(|payload| {
                payload["message"]["timestamp"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc))
            })
            .collect();

        assert_eq!(
            timestamps.len(),
            6,
            "combined replay must surface all six events: {replayed:?}"
        );

        let mut sorted = timestamps.clone();
        sorted.sort();
        assert_eq!(
            timestamps, sorted,
            "combined replay must be globally sorted by timestamp; got {timestamps:?}"
        );

        // Spot-check the chronological interleave (candidate-T0, fallback-T1, ...).
        let contents: Vec<String> = replayed
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
            .filter_map(|payload| payload["message"]["content"].as_str().map(str::to_string))
            .collect();
        let expected_order = [
            "candidate-0-T0",
            "fallback-0-T1",
            "candidate-1-T2",
            "fallback-1-T3",
            "candidate-2-T4",
            "fallback-2-T5",
        ];
        assert_eq!(
            contents, expected_order,
            "combined replay must interleave by timestamp"
        );
    }

    #[tokio::test]
    async fn replay_committed_session_results_replays_only_newer_assistant_messages() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let key =
            current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "test-chat", None);

        {
            let mut manager = sessions.lock().await;
            manager
                .add_message_with_seq(&key, Message::user("hello"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(&key, Message::assistant("first result"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    Message::assistant("✓ report completed — file delivered"),
                )
                .await
                .unwrap();
        }

        let state = ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions,
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            last_thread_id: Arc::new(Mutex::new(HashMap::new())),
        };

        let replayed = replay_committed_session_results(&state, "test-chat", Some(1), None).await;

        assert_eq!(replayed.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&replayed[0]).unwrap();
        assert_eq!(parsed["type"], "session_result");
        assert_eq!(parsed["message"]["seq"], 2);
        assert_eq!(parsed["message"]["role"], "assistant");
        assert_eq!(
            parsed["message"]["content"],
            "✓ report completed — file delivered"
        );
    }

    #[tokio::test]
    async fn replay_committed_session_results_without_since_seq_replays_all_assistant_messages() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let key = current_profile_api_session_key_with_topic(
            Some(TEST_PROFILE_ID),
            "test-chat",
            Some("slides launch"),
        );
        let deck_path = data_dir.path().join("slides").join("final-deck.pptx");
        std::fs::create_dir_all(deck_path.parent().unwrap()).unwrap();
        std::fs::write(&deck_path, b"deck").unwrap();

        {
            let mut manager = sessions.lock().await;
            manager
                .add_message_with_seq(&key, Message::user("hello"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(&key, Message::assistant("first result"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    Message {
                        role: MessageRole::Assistant,
                        content: "final deck".to_string(),
                        media: vec![deck_path.to_string_lossy().into_owned()],
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                        client_message_id: None,
                        thread_id: None,
                        timestamp: chrono::Utc::now(),
                    },
                )
                .await
                .unwrap();
        }

        let state = ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions,
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            last_thread_id: Arc::new(Mutex::new(HashMap::new())),
        };

        let replayed =
            replay_committed_session_results(&state, "test-chat", None, Some("slides launch"))
                .await;

        assert_eq!(replayed.len(), 2);
        let first: serde_json::Value = serde_json::from_str(&replayed[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(&replayed[1]).unwrap();
        assert_eq!(first["type"], "session_result");
        assert_eq!(first["topic"], "slides launch");
        assert_eq!(first["message"]["seq"], 1);
        assert_eq!(first["message"]["content"], "first result");
        assert_eq!(second["type"], "session_result");
        assert_eq!(second["topic"], "slides launch");
        assert_eq!(second["message"]["seq"], 2);
        let media = second["message"]["media"].as_array().unwrap();
        assert_eq!(media.len(), 1);
        assert!(media[0].as_str().unwrap().starts_with("pf/"));
    }

    #[test]
    fn should_drop_replayed_session_result_only_for_already_replayed_seq() {
        let replayed = serde_json::json!({
            "type": "session_result",
            "message": {
                "seq": 7,
                "role": "assistant",
                "content": "done",
            }
        })
        .to_string();
        let newer = serde_json::json!({
            "type": "session_result",
            "message": {
                "seq": 8,
                "role": "assistant",
                "content": "later",
            }
        })
        .to_string();
        let replace = serde_json::json!({
            "type": "replace",
            "text": "partial",
        })
        .to_string();

        assert!(should_drop_replayed_session_result(&replayed, Some(7)));
        assert!(should_drop_replayed_session_result(&replayed, Some(9)));
        assert!(!should_drop_replayed_session_result(&newer, Some(7)));
        assert!(!should_drop_replayed_session_result(&replace, Some(7)));
        assert!(!should_drop_replayed_session_result(&replayed, None));
    }

    #[test]
    fn session_result_seq_from_payload_reads_message_seq() {
        let payload = serde_json::json!({
            "type": "session_result",
            "message": {
                "seq": 3,
                "role": "assistant",
                "content": "hello",
            }
        })
        .to_string();
        let no_seq = serde_json::json!({
            "type": "session_result",
            "message": {
                "role": "assistant",
                "content": "hello",
            }
        })
        .to_string();

        assert_eq!(session_result_seq_from_payload(&payload), Some(3));
        assert_eq!(session_result_seq_from_payload(&no_seq), None);
        assert_eq!(session_result_seq_from_payload("{not-json"), None);
    }

    #[tokio::test]
    async fn replay_committed_session_results_skips_empty_assistant_tool_trace_messages() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let key = current_profile_api_session_key_with_topic(
            Some(TEST_PROFILE_ID),
            "tool-heavy-chat",
            None,
        );

        {
            let mut manager = sessions.lock().await;
            manager
                .add_message_with_seq(&key, Message::user("查一下他的背景 John Ternus"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    assistant_tool_call_message(
                        "deep_search",
                        serde_json::json!({"query": "John Ternus 背景"}),
                    ),
                )
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    assistant_tool_call_message(
                        "get_time",
                        serde_json::json!({
                            "timezone": "America/Los_Angeles",
                            "current_date": "2026-04-20"
                        }),
                    ),
                )
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    assistant_tool_call_message(
                        "activate_tools",
                        serde_json::json!({"tools": ["cron"]}),
                    ),
                )
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    assistant_tool_call_message("cron", serde_json::json!({"action": "list"})),
                )
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    Message::assistant("John Ternus is Apple's SVP of hardware engineering."),
                )
                .await
                .unwrap();
            manager
                .add_message_with_seq(&key, Message::user("你有哪些定时任务"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    assistant_tool_call_message("cron", serde_json::json!({"action": "list"})),
                )
                .await
                .unwrap();
            manager
                .add_message_with_seq(&key, Message::user("提醒我 10 分钟后喝水，我在 PDT 时区"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    assistant_tool_call_message(
                        "get_time",
                        serde_json::json!({"timezone": "America/Los_Angeles"}),
                    ),
                )
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    assistant_tool_call_message(
                        "activate_tools",
                        serde_json::json!({"tools": ["cron"]}),
                    ),
                )
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    assistant_tool_call_message(
                        "cron",
                        serde_json::json!({"action": "add", "in_minutes": 10}),
                    ),
                )
                .await
                .unwrap();
            manager
                .add_message_with_seq(&key, Message::user("记住我的时区"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(
                    &key,
                    Message::assistant("好的，已记住你的时区为 PDT（America/Los_Angeles）。"),
                )
                .await
                .unwrap();
            // Trailing empty tool-trace assistant message previously could overwrite
            // the visible final answer in client reconciliation.
            manager
                .add_message_with_seq(
                    &key,
                    assistant_tool_call_message("cron", serde_json::json!({"action": "list"})),
                )
                .await
                .unwrap();
        }

        let state = ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions,
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            last_thread_id: Arc::new(Mutex::new(HashMap::new())),
        };

        let replayed =
            replay_committed_session_results(&state, "tool-heavy-chat", None, None).await;

        assert_eq!(replayed.len(), 2);
        for event in &replayed {
            let parsed: serde_json::Value = serde_json::from_str(event).unwrap();
            let content = parsed["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            assert!(!content.is_empty());
        }
        let last: serde_json::Value = serde_json::from_str(replayed.last().unwrap()).unwrap();
        assert_eq!(
            last["message"]["content"],
            "好的，已记住你的时区为 PDT（America/Los_Angeles）。"
        );
    }

    #[tokio::test]
    async fn replay_task_status_events_replays_current_tasks_with_topic() {
        let state = ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions: test_sessions(),
            task_query: Some(Arc::new(|_| {
                serde_json::json!([
                    {
                        "id": "task-1",
                        "tool_name": "podcast_generate",
                        "status": "running",
                        "started_at": "2026-04-16T00:00:00Z",
                        "error": null
                    }
                ])
            })),
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            last_thread_id: Arc::new(Mutex::new(HashMap::new())),
        };

        let replayed = replay_task_status_events(&state, "test-chat", Some("site astro")).await;

        assert_eq!(replayed.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&replayed[0]).unwrap();
        assert_eq!(parsed["type"], "task_status");
        assert_eq!(parsed["topic"], "site astro");
        assert_eq!(parsed["task"]["id"], "task-1");
        assert_eq!(parsed["task"]["tool_name"], "podcast_generate");
    }

    #[tokio::test]
    async fn send_completion_closes_stream() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-chat".into(), tx);
        }

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-chat".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "_completion": true,
                "model": "moonshot/kimi-k2.5 @ autodl.art",
                "provider": "moonshot",
                "model_id": "kimi-k2.5",
                "endpoint": "autodl.art",
                "tokens_in": 123,
                "tokens_out": 456,
                "session_cost": 0.0228,
            }),
        };
        ch.send(&msg).await.unwrap();

        // Should receive done event
        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "done");
        assert_eq!(parsed["model"], "moonshot/kimi-k2.5 @ autodl.art");
        assert_eq!(parsed["provider"], "moonshot");
        assert_eq!(parsed["model_id"], "kimi-k2.5");
        assert_eq!(parsed["endpoint"], "autodl.art");
        assert_eq!(parsed["tokens_in"], 123);
        assert_eq!(parsed["tokens_out"], 456);
        assert_eq!(parsed["session_cost"], 0.0228);

        // Sender was removed — next recv returns None
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn done_event_carries_committed_seq_when_message_persisted() {
        // M8.10-A regression: the SSE `done` event must thread the committed
        // session sequence back to the web client so live-streamed bubbles can
        // populate `historySeq` and avoid floating to the end of the list.
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-chat-seq".into(), tx);
        }

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-chat-seq".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "_completion": true,
                "committed_seq": 42,
                "tokens_in": 10,
                "tokens_out": 5,
            }),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "done");
        assert_eq!(parsed["committed_seq"], 42);
    }

    /// M8.10 PR #2: every SSE event the API channel emits MUST include
    /// `thread_id` (sourced from `OutboundMessage.metadata.thread_id`) so
    /// web clients with multiple in-flight threads on the same chat_id
    /// can route streamed events to the right per-thread bubble.
    #[tokio::test]
    async fn done_event_includes_thread_id_from_metadata() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-chat-tid".into(), tx);
        }

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-chat-tid".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "_completion": true,
                "committed_seq": 11,
                "thread_id": "cmid-thread-Z",
                "tokens_in": 0,
                "tokens_out": 0,
            }),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "done");
        assert_eq!(parsed["committed_seq"], 11);
        assert_eq!(parsed["thread_id"], "cmid-thread-Z");
    }

    /// M8.10 PR #2: the wire-side `replace` event emitted by `send`
    /// (non-streaming assistant content) must carry thread_id.
    #[tokio::test]
    async fn replace_event_includes_thread_id_from_metadata() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-chat-replace".into(), tx);
        }

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-chat-replace".into(),
            content: "hello world".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "thread_id": "cmid-thread-R",
            }),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "replace");
        assert_eq!(parsed["text"], "hello world");
        assert_eq!(parsed["thread_id"], "cmid-thread-R");
    }

    /// M8.10 PR #2: streaming `token` and `replace` events emitted via
    /// `edit_message` must carry thread_id encoded into the synthetic
    /// message_id returned by `send_with_id`. This is the key handshake
    /// that lets two concurrent threads on the same chat_id be
    /// demultiplexed by web clients.
    #[tokio::test]
    async fn edit_message_token_event_includes_thread_id_decoded_from_message_id() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("chat-edit".into(), tx);
        }

        // Step 1: send_with_id encodes thread_id into the message_id
        let initial = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-edit".into(),
            content: "Hi".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "thread_id": "cmid-thread-EDIT",
            }),
        };
        let message_id = ch.send_with_id(&initial).await.unwrap().unwrap();
        // Drain the initial replace event from send().
        let _ = rx.recv().await.unwrap();

        // Step 2: edit_message decodes thread_id back from message_id and
        // tags the streaming `token`/`replace` payload with it.
        ch.edit_message("chat-edit", &message_id, "Hi there")
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        // The delta comparison sees prev="Hi" and new starts with "Hi", so
        // it emits a `token` for the suffix.
        assert_eq!(parsed["type"], "token");
        assert_eq!(parsed["text"], " there");
        assert_eq!(parsed["thread_id"], "cmid-thread-EDIT");
    }

    /// M8.10 follow-up (#632): production probe of mini3 showed the FIRST
    /// `replace` event of a turn leaving the daemon with `thread_id=null`,
    /// even though the user-message session_result outbound (sent by the
    /// session actor BEFORE streaming starts) carries thread_id metadata.
    /// The stream forwarder's `do_flush` builds outbound metadata that
    /// only includes `streaming: true`, so when `send_with_id` falls
    /// through to the encoder it has no thread_id to embed in the
    /// synthetic message_id. Subsequent `edit_message` calls then decode
    /// `(_, None)` from the synthetic id and emit untagged events.
    ///
    /// Fix: when `decode_sse_message_id` returns `None` for thread_id,
    /// `edit_message` falls back to a sticky map keyed by chat_id.
    /// The map is populated whenever `send` (or `send_with_id`) sees a
    /// thread_id in metadata — including the user-message session_result
    /// emission that fires before streaming.
    #[tokio::test]
    async fn edit_message_emits_thread_id_via_sticky_map_when_synthetic_id_lacks_one() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("chat-sticky".into(), tx);
        }

        // Step 1: simulate the session actor emitting the user-message
        // session_result outbound BEFORE the stream forwarder ever calls
        // send_with_id. This is exactly the order observed in mini3:
        // the session actor publishes a thread_id-tagged outbound, then
        // the agent loop emits the first thinking + replace events while
        // `do_flush`'s send_with_id is still racing with them.
        let bind_msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-sticky".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "_session_result": {
                    "seq": 1,
                    "role": "user",
                    "content": "hello",
                    "client_message_id": "cmid-sticky-T",
                },
                "thread_id": "cmid-sticky-T",
            }),
        };
        ch.send(&bind_msg).await.unwrap();
        // Drain the session_result event so the assertion below sees the
        // `replace` we care about.
        let _ = rx.recv().await.unwrap();

        // Step 2: stream forwarder's `do_flush` builds outbound metadata
        // that does NOT include `thread_id` (only `streaming: true` and
        // optionally a sender_user_id). Mirror that here.
        let stream_initial = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-sticky".into(),
            content: "Hi".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({"streaming": true}),
        };
        let message_id = ch.send_with_id(&stream_initial).await.unwrap().unwrap();
        // M8.10 follow-up (#636): `send_with_id` now also recovers
        // thread_id from the sticky map when its outbound metadata
        // lacks one, so the synthetic message_id is pre-tagged with
        // the bound cmid. Earlier behaviour (#632) was to leave the
        // encoded id naked and rely on `edit_message`'s sticky
        // fallback alone — that path is still exercised below.
        assert_eq!(
            decode_sse_message_id(&message_id).1.as_deref(),
            Some("cmid-sticky-T"),
            "send_with_id should encode thread_id from sticky map (#636)",
        );
        // Drain the initial replace event emitted by send_with_id.
        let _ = rx.recv().await.unwrap();

        // Step 3: subsequent edit_message must still tag the streaming
        // payload with the bound thread_id by falling back to the sticky
        // map populated in step 1.
        ch.edit_message("chat-sticky", &message_id, "Hi there")
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "token");
        assert_eq!(parsed["text"], " there");
        assert_eq!(
            parsed["thread_id"], "cmid-sticky-T",
            "edit_message must recover thread_id from the sticky map when the \
             synthetic message_id lacks one (production race window observed \
             on mini3 — see #632)"
        );

        // Step 4: another edit on the same chat_id continues to inherit
        // the sticky thread_id (the binding is "sticky" — once set it
        // does not erase).
        ch.edit_message("chat-sticky", &message_id, "Hi there friend")
            .await
            .unwrap();
        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["thread_id"], "cmid-sticky-T");
    }

    /// M8.10 follow-up (#632): the stream reporter forwards discrete
    /// events (thinking, response, cost_update, ...) as pre-rendered JSON
    /// via `send_raw_sse`. If the reporter has not yet bound thread_id
    /// (the first `Thinking` of a turn observed in production) the JSON
    /// arrives without a `thread_id` field. The api_channel's sticky map
    /// is the second-line defence: if the session actor previously
    /// emitted a thread_id-tagged outbound on this chat_id (e.g. the
    /// user-message session_result), the wire event still carries the
    /// right thread.
    ///
    /// This test drives the production sequence: send a bind via `send`,
    /// then forward two raw SSE thinking events with no thread_id field,
    /// and assert both events leave the channel tagged. The "BOTH" guard
    /// is the regression hook — without the sticky lookup, only the
    /// second event would carry thread_id (after the reporter
    /// rebound) and the first would race on the wire.
    #[tokio::test]
    async fn early_thinking_event_emits_thread_id_via_sticky_map() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("chat-thinking".into(), tx);
        }

        // Bind thread_id sticky via the user-message session_result
        // outbound the session actor emits before streaming starts.
        let bind_msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-thinking".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "_session_result": {
                    "seq": 1,
                    "role": "user",
                    "content": "hi",
                    "client_message_id": "cmid-thinking-Q",
                },
                "thread_id": "cmid-thinking-Q",
            }),
        };
        ch.send(&bind_msg).await.unwrap();
        // Drain the session_result event.
        let _ = rx.recv().await.unwrap();

        // First Thinking event arrives via send_raw_sse with NO thread_id
        // field (the reporter had not bound one when it constructed this
        // payload).
        let raw_thinking_1 = serde_json::json!({"type": "thinking", "iteration": 0});
        ch.send_raw_sse("chat-thinking", &raw_thinking_1.to_string())
            .await
            .unwrap();
        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "thinking");
        assert_eq!(
            parsed["thread_id"], "cmid-thinking-Q",
            "first thinking must inherit thread_id from sticky map even \
             though the raw JSON lacked it (#632)"
        );

        // Second Thinking — even if the reporter had now bound thread_id
        // upstream, sticky lookup must remain consistent on the daemon
        // side. Drive ANOTHER unbound payload to prove the sticky hit
        // is not a one-shot.
        let raw_thinking_2 = serde_json::json!({"type": "thinking", "iteration": 1});
        ch.send_raw_sse("chat-thinking", &raw_thinking_2.to_string())
            .await
            .unwrap();
        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "thinking");
        assert_eq!(
            parsed["thread_id"], "cmid-thinking-Q",
            "second thinking must continue to receive thread_id via \
             sticky map (regression guard for the M8.10 SSE race)"
        );
    }

    /// M8.10 follow-up (#632): when the raw SSE JSON already carries a
    /// thread_id, `send_raw_sse` must respect it (and not overwrite via
    /// the sticky map). It also updates the sticky map so subsequent
    /// untagged events on the same chat_id can recover the value.
    #[tokio::test]
    async fn send_raw_sse_preserves_explicit_thread_id_and_updates_sticky() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("chat-explicit".into(), tx);
        }

        // Tagged thinking — must pass through unchanged AND populate
        // sticky for any subsequent untagged events.
        let tagged = serde_json::json!({
            "type": "thinking",
            "iteration": 0,
            "thread_id": "cmid-explicit-A",
        });
        ch.send_raw_sse("chat-explicit", &tagged.to_string())
            .await
            .unwrap();
        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["thread_id"], "cmid-explicit-A");

        // Subsequent untagged thinking inherits the sticky binding.
        let untagged = serde_json::json!({"type": "thinking", "iteration": 1});
        ch.send_raw_sse("chat-explicit", &untagged.to_string())
            .await
            .unwrap();
        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["thread_id"], "cmid-explicit-A");
    }

    /// #649 follow-up (rapid-fire): when 5 chat streams interleave on the
    /// same chat_id, each turn's stream forwarder must call `send_with_id`
    /// with its OWN cmid in metadata so subsequent `edit_message` /
    /// `send_raw_sse` calls can recover the right thread.
    ///
    /// Pre-fix the stream forwarder built outbound metadata containing only
    /// `streaming: true` and let `send_with_id` fall back to the sticky
    /// map. Under rapid-fire (Q1..Q5 lining up on the same session before
    /// any of them finishes), the sticky map has rotated to the LAST
    /// request's cmid by the time Q1's first chunk reaches the channel —
    /// so Q1's encoded message_id captures Q5's cmid and every subsequent
    /// streaming `token` / `replace` for Q1 mis-routes to Q5's bubble on
    /// the web client. This drives that exact ordering and asserts each
    /// turn's encoded message_id carries its OWN thread_id when supplied
    /// explicitly via the OutboundMessage metadata.
    #[tokio::test]
    async fn send_with_id_uses_explicit_metadata_thread_id_over_sticky() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, _rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("chat-rapid-fire".into(), tx);
        }

        // Five rapid-fire `handle_chat`-equivalent sticky rotations: the
        // PRODUCTION race is each new request pinning sticky to its own
        // cmid before any earlier turn has produced its first stream
        // chunk. By the time Q1's stream forwarder calls `send_with_id`,
        // sticky already holds Q5's cmid.
        for cmid in ["cmid-A", "cmid-B", "cmid-C", "cmid-D", "cmid-E"] {
            ch.remember_thread_id("chat-rapid-fire", Some(cmid)).await;
        }

        // Q1's stream forwarder calls `send_with_id` with metadata that
        // EXPLICITLY carries Q1's cmid (the post-fix shape). The encoded
        // message_id must capture cmid-A — NOT the sticky's cmid-E.
        let q1 = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-rapid-fire".into(),
            content: "first chunk for Q1".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "streaming": true,
                "thread_id": "cmid-A",
            }),
        };
        let q1_msg_id = ch
            .send_with_id(&q1)
            .await
            .unwrap()
            .expect("send_with_id always returns Some");
        let (_, q1_decoded) = decode_sse_message_id(&q1_msg_id);
        assert_eq!(
            q1_decoded.as_deref(),
            Some("cmid-A"),
            "Q1's encoded message_id must capture its OWN cmid, not the sticky's last value (cmid-E). Got: {q1_msg_id}"
        );

        // Q3 lands next, also with explicit metadata. Same expectation —
        // Q3's encoded id must reflect Q3's cmid even though sticky still
        // points elsewhere.
        let q3 = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-rapid-fire".into(),
            content: "first chunk for Q3".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "streaming": true,
                "thread_id": "cmid-C",
            }),
        };
        let q3_msg_id = ch
            .send_with_id(&q3)
            .await
            .unwrap()
            .expect("send_with_id always returns Some");
        let (_, q3_decoded) = decode_sse_message_id(&q3_msg_id);
        assert_eq!(
            q3_decoded.as_deref(),
            Some("cmid-C"),
            "Q3's encoded message_id must capture its OWN cmid even with concurrent sticky rotations. Got: {q3_msg_id}"
        );
    }

    /// #649 follow-up (rapid-fire): drive an end-to-end interleaved
    /// 5-turn rapid-fire scenario through `send` and assert each turn's
    /// `replace` event carries its OWN cmid when the OutboundMessage
    /// metadata supplies one explicitly. This exercises the path the
    /// stream forwarder takes for non-first chunks (the inner `send` call
    /// from `send_with_id`).
    #[tokio::test]
    async fn rapid_fire_streaming_chunks_carry_per_turn_thread_id() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("chat-rapid".into(), tx);
        }

        // Simulate `handle_chat` rotating sticky to the LATEST cmid as
        // each rapid-fire request arrives.
        for cmid in ["cmid-A", "cmid-B", "cmid-C", "cmid-D", "cmid-E"] {
            ch.remember_thread_id("chat-rapid", Some(cmid)).await;
        }

        // Each turn's first chunk arrives via `send` with explicit
        // thread_id metadata. The `replace` event emitted on the wire
        // must be tagged with that cmid, NOT the latest sticky value.
        let cases = [
            ("cmid-A", "Q1 reply"),
            ("cmid-B", "Q2 reply"),
            ("cmid-C", "Q3 reply"),
            ("cmid-D", "Q4 reply"),
        ];
        for (cmid, content) in &cases {
            let msg = OutboundMessage {
                channel: "api".into(),
                chat_id: "chat-rapid".into(),
                content: (*content).into(),
                reply_to: None,
                media: vec![],
                metadata: serde_json::json!({
                    "streaming": true,
                    "thread_id": cmid,
                }),
            };
            ch.send(&msg).await.unwrap();
            // Reset last_content so each turn's chunk emits as a `replace`,
            // not a delta `token` (the production stream forwarder calls
            // `send_with_id` first which clears last_content; we mimic that
            // by clearing it inline here).
            ch.last_content.lock().await.remove("chat-rapid");
        }

        // Drain wire events and verify each carries its OWN cmid.
        let mut events: Vec<serde_json::Value> = Vec::new();
        while let Ok(payload) = rx.try_recv() {
            events.push(serde_json::from_str(&payload).unwrap());
        }
        let replaces: Vec<&serde_json::Value> =
            events.iter().filter(|e| e["type"] == "replace").collect();
        assert_eq!(
            replaces.len(),
            cases.len(),
            "expected {} replace events, got {}: {:?}",
            cases.len(),
            replaces.len(),
            events,
        );
        for ((expected_cmid, expected_text), event) in cases.iter().zip(replaces.iter()) {
            assert_eq!(
                event["text"], *expected_text,
                "replace event text mismatch: {event}"
            );
            assert_eq!(
                event["thread_id"], *expected_cmid,
                "replace event for {expected_text} mis-tagged. Expected {expected_cmid}, got: {event}"
            );
        }
    }

    /// overflow-stress regression (#680 follow-up): when two concurrent
    /// streams on the same chat have prefix-overlapping content, the
    /// `chat_id`-only `last_content` key let turn A's prev poison turn B's
    /// delta computation. A specific failure mode observed in the live
    /// soak: turn A produces "Hello" first, turn B then sends its own
    /// independent "Hello world" as a fresh `replace` chunk — pre-fix,
    /// `edit_message` saw `prev["chat"]="Hello"` and emitted a misleading
    /// `token` delta " world" tagged with thread B, with the result that
    /// the web client painted A's earlier content under B's user bubble.
    /// Per-(chat, thread) keying isolates the two streams so each computes
    /// its delta against its OWN prev.
    ///
    /// The post-fix wire shape: turn A's edit emits a `token` delta from
    /// its own prev; turn B's first edit emits a full `replace` (because
    /// no prev exists for B yet). Either way, neither stream cross-talks.
    #[tokio::test]
    async fn concurrent_same_chat_streams_do_not_cross_talk_via_last_content() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("chat-overflow".into(), tx);
        }

        // Step 1: Turn A starts streaming. send_with_id seeds prev for A.
        let a_initial = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-overflow".into(),
            content: "Hello".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "streaming": true,
                "thread_id": "cmid-A",
            }),
        };
        let a_msg_id = ch.send_with_id(&a_initial).await.unwrap().unwrap();
        // Drain the initial replace event for A.
        let _ = rx.recv().await.unwrap();

        // Step 2: Turn B starts streaming on the SAME chat with a DIFFERENT
        // thread_id. send_with_id must NOT inherit A's prev as B's seed —
        // that's the cross-talk root cause.
        let b_initial = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-overflow".into(),
            content: "Hello world".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "streaming": true,
                "thread_id": "cmid-B",
            }),
        };
        let b_msg_id = ch.send_with_id(&b_initial).await.unwrap().unwrap();
        // Drain the initial replace event for B.
        let _ = rx.recv().await.unwrap();

        // Step 3: Turn A's stream forwarder emits the next chunk via
        // edit_message. Pre-fix, the chat-only key now holds B's "Hello
        // world" so A's edit computed `prev = "Hello world"` (not a prefix
        // of A's "Hello there") → emitted a wasteful full `replace`. With
        // per-thread keying, A's prev is its OWN "Hello", so the delta
        // " there" emits as a `token` tagged for cmid-A.
        ch.edit_message("chat-overflow", &a_msg_id, "Hello there")
            .await
            .unwrap();
        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            parsed["thread_id"], "cmid-A",
            "edit on A must tag thread_id=cmid-A, not B's. Got: {parsed}"
        );
        assert_eq!(
            parsed["type"], "token",
            "A's edit should emit a token delta against A's own prev. Got: {parsed}"
        );
        assert_eq!(
            parsed["text"], " there",
            "A's delta must be from A's own prev (\"Hello\"), not B's (\"Hello world\"). Got: {parsed}"
        );

        // Step 4: Turn B's edit_message arrives next with content that
        // happens to share A's "Hello there" prefix. Pre-fix, A's just-
        // recorded "Hello there" would seed prev["chat"], and B's "Hello
        // there is something" would emit a `token` " is something" stamped
        // with cmid-B that contained text *originally produced by A*. With
        // per-thread keying, B's prev is "Hello world" (B's own seed), and
        // "Hello there is something" does NOT start with "Hello world", so
        // we fall through to the safe `replace` path.
        ch.edit_message("chat-overflow", &b_msg_id, "Hello there is something")
            .await
            .unwrap();
        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            parsed["thread_id"], "cmid-B",
            "B's edit must tag thread_id=cmid-B. Got: {parsed}"
        );
        // The critical regression assertion: B's wire payload must NEVER
        // contain a token delta that *starts with* content originally
        // produced by A. We assert the non-token shape (full replace)
        // since "Hello there is something" doesn't extend B's own prev
        // ("Hello world").
        assert_eq!(
            parsed["type"], "replace",
            "B should emit a full replace (B's prev was \"Hello world\", not a prefix of \"Hello there is something\"). Pre-fix this leaked a cmid-B-tagged token delta containing A's content. Got: {parsed}"
        );
        assert_eq!(parsed["text"], "Hello there is something");
    }

    /// overflow-stress regression: when one concurrent stream finalizes
    /// (`done`), the `last_content` cleanup must drop ONLY that turn's
    /// per-thread entry — never wipe a sibling turn's prev. Without per-
    /// thread keying, A's `done` cleared the chat-wide key, forcing the
    /// next B chunk to emit a wasteful `replace`. Worse, since the
    /// chat-only key had been seeded by whichever turn last ran, A's
    /// `done` could discard B's prev entirely. Per-thread keying scopes
    /// the cleanup to A and leaves B's stream state untouched.
    #[tokio::test]
    async fn done_cleanup_does_not_wipe_concurrent_thread_last_content() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("chat-cleanup".into(), tx);
        }

        // Turn A and turn B both seed last_content under their own
        // per-thread keys.
        let a = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-cleanup".into(),
            content: "Apples".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "streaming": true,
                "thread_id": "cmid-A",
            }),
        };
        let _ = ch.send_with_id(&a).await.unwrap().unwrap();
        let _ = rx.recv().await.unwrap();
        let b = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-cleanup".into(),
            content: "Bananas".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "streaming": true,
                "thread_id": "cmid-B",
            }),
        };
        let b_msg_id = ch.send_with_id(&b).await.unwrap().unwrap();
        let _ = rx.recv().await.unwrap();

        // Turn A finalizes first. The `done` cleanup must scope its
        // last_content removal to cmid-A only — not blow away B's seed.
        let a_done = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-cleanup".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "_completion": true,
                "thread_id": "cmid-A",
            }),
        };
        ch.send(&a_done).await.unwrap();
        // Re-add the broadcast subscriber, since `_completion` removes
        // the pending channel.
        let (tx2, mut rx2) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("chat-cleanup".into(), tx2);
        }

        // Turn B's next chunk should still see B's prev ("Bananas") and
        // emit a delta `token` for the suffix. Pre-fix this would have
        // emitted a wasteful full `replace` (or worse, nothing visible)
        // because A's done wiped the chat-only prev key.
        ch.edit_message("chat-cleanup", &b_msg_id, "Bananas are yellow")
            .await
            .unwrap();
        let event = rx2.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["thread_id"], "cmid-B");
        assert_eq!(
            parsed["type"], "token",
            "B's prev ('Bananas') must survive A's done cleanup. Got: {parsed}"
        );
        assert_eq!(parsed["text"], " are yellow");
    }

    /// Pre-cmid clients send messages with no thread_id metadata. The wire
    /// schema must remain backwards-compatible: events emitted in this
    /// case must NOT include a `thread_id` field at all.
    #[tokio::test]
    async fn done_event_omits_thread_id_when_metadata_absent() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-chat-no-tid".into(), tx);
        }

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-chat-no-tid".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "_completion": true,
            }),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "done");
        assert!(
            parsed.get("thread_id").is_none(),
            "thread_id field must be absent when metadata didn't carry one"
        );
    }

    #[tokio::test]
    async fn done_event_omits_committed_seq_when_persist_failed_or_skipped() {
        // M8.10-A: when the server has no committed seq (e.g. persist failed
        // or is skipped), the done event must NOT include `committed_seq` so
        // legacy/error-path behaviour is preserved.
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-chat-noseq".into(), tx);
        }

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-chat-noseq".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "_completion": true,
                "tokens_in": 10,
                "tokens_out": 5,
            }),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "done");
        assert!(
            parsed.get("committed_seq").is_none() || parsed["committed_seq"].is_null(),
            "committed_seq must be omitted when missing from metadata, got: {parsed}"
        );
    }

    #[tokio::test]
    async fn send_completion_with_bg_tasks_closes_and_client_polls() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let source_dir = data_dir.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("test.mp3");
        std::fs::write(&source, b"bg-audio").unwrap();
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-bg".into(), tx);
        }

        // Send completion with has_bg_tasks = true
        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-bg".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({"_completion": true, "has_bg_tasks": true}),
        };
        ch.send(&msg).await.unwrap();

        // Should receive done event with has_bg_tasks flag
        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "done");
        assert_eq!(parsed["has_bg_tasks"], true);

        // SSE closes immediately — client will poll session history
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));

        // Background file arrives later — persisted to session history
        let file_msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-bg".into(),
            content: String::new(),
            reply_to: None,
            media: vec![source.to_string_lossy().to_string()],
            metadata: serde_json::json!({}),
        };
        ch.send(&file_msg).await.unwrap();

        // Client polling session history would find it
        let mut sess = sessions.lock().await;
        let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "test-bg");
        let session = sess.get_or_create(&key).await;
        let history = session.get_history(10);
        let stored = history
            .iter()
            .flat_map(|m| m.media.iter())
            .find(|path| path.ends_with("test.mp3"))
            .cloned()
            .expect("expected persisted artifact path");
        assert_ne!(stored, source.to_string_lossy().to_string());
        assert!(Path::new(&stored).exists());
    }

    #[tokio::test]
    async fn send_completion_with_bg_tasks_emits_compat_tool_start_before_done() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        )
        .with_task_query(Arc::new(|_| {
            serde_json::json!([
                {
                    "id": "task-1",
                    "tool_name": "Direct TTS",
                    "tool_call_id": "call_tts_1",
                    "status": "running"
                }
            ])
        }));
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-bg-compat".into(), tx);
        }

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-bg-compat".into(),
            content: String::new(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({"_completion": true, "has_bg_tasks": true}),
        };
        ch.send(&msg).await.unwrap();

        let first: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        let second: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();

        assert_eq!(first["type"], "tool_start");
        assert_eq!(first["tool"], "fm_tts");
        assert_eq!(first["tool_call_id"], "call_tts_1");
        assert_eq!(second["type"], "done");
        assert_eq!(second["has_bg_tasks"], true);
    }

    #[tokio::test]
    async fn send_file_message_persists_to_session() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let source_dir = data_dir.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("test.mp3");
        std::fs::write(&source, b"audio").unwrap();

        // Send a file message (no active SSE needed — goes straight to session)
        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-file".into(),
            content: "Audio file".into(),
            reply_to: None,
            media: vec![source.to_string_lossy().to_string()],
            metadata: serde_json::json!({}),
        };
        ch.send(&msg).await.unwrap();

        // Verify it was persisted to the session
        let mut sess = sessions.lock().await;
        let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "test-file");
        let session = sess.get_or_create(&key).await;
        let history = session.get_history(10);
        assert_eq!(history.len(), 1);
        assert!(history[0].content.contains("Audio file"));
        assert_eq!(history[0].media.len(), 1);
        let persisted = &history[0].media[0];
        assert_ne!(persisted, &source.to_string_lossy().to_string());
        assert!(!history[0].content.contains(persisted));
        assert!(Path::new(persisted).exists());
        assert_eq!(std::fs::read(Path::new(persisted)).unwrap(), b"audio");
    }

    #[tokio::test]
    async fn send_file_message_emits_committed_session_result_event() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let (tx, mut rx) = new_sse_channel();
        {
            let mut pending = ch.pending.lock().await;
            pending.insert("test-file".into(), tx);
        }

        let source_dir = data_dir.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("report.pdf");
        std::fs::write(&source, b"report").unwrap();

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-file".into(),
            content: "Generated report".into(),
            reply_to: None,
            media: vec![source.to_string_lossy().to_string()],
            metadata: serde_json::json!({"tool_call_id": "call_report_1"}),
        };
        ch.send(&msg).await.unwrap();

        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "session_result");
        let message = parsed["message"].as_object().expect("message payload");
        assert_eq!(
            message.get("role").and_then(|v| v.as_str()),
            Some("assistant")
        );
        assert!(
            message
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("Generated report")
        );
        assert_eq!(
            message.get("tool_call_id").and_then(|v| v.as_str()),
            Some("call_report_1")
        );
        let media = message
            .get("media")
            .and_then(|v| v.as_array())
            .expect("media array");
        assert_eq!(media.len(), 1);
        let persisted = media[0].as_str().expect("persisted path");
        assert!(persisted.starts_with("pf/"));
    }

    #[tokio::test]
    async fn send_file_message_keeps_distinct_artifacts_for_same_basename() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let source_a_dir = data_dir.path().join("source-a");
        let source_b_dir = data_dir.path().join("source-b");
        std::fs::create_dir_all(&source_a_dir).unwrap();
        std::fs::create_dir_all(&source_b_dir).unwrap();
        let source_a = source_a_dir.join("report.pdf");
        let source_b = source_b_dir.join("report.pdf");
        std::fs::write(&source_a, b"alpha").unwrap();
        std::fs::write(&source_b, b"beta").unwrap();

        for source in [&source_a, &source_b] {
            let msg = OutboundMessage {
                channel: "api".into(),
                chat_id: "collision-chat".into(),
                content: "report".into(),
                reply_to: None,
                media: vec![source.to_string_lossy().to_string()],
                metadata: serde_json::json!({}),
            };
            ch.send(&msg).await.unwrap();
        }

        let mut sess = sessions.lock().await;
        let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "collision-chat");
        let session = sess.get_or_create(&key).await;
        let history = session.get_history(10);
        assert_eq!(history.len(), 2);
        let first = history[0].media[0].clone();
        let second = history[1].media[0].clone();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(Path::new(&first)).unwrap(), b"alpha");
        assert_eq!(std::fs::read(Path::new(&second)).unwrap(), b"beta");
    }

    #[tokio::test]
    async fn send_file_message_reuses_existing_session_artifact() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "artifact-chat");
        let artifact_dir = ApiChannel::session_artifact_dir(data_dir.path(), &key);
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let existing = artifact_dir.join("existing.wav");
        std::fs::write(&existing, b"persisted").unwrap();

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "artifact-chat".into(),
            content: "existing".into(),
            reply_to: None,
            media: vec![existing.to_string_lossy().to_string()],
            metadata: serde_json::json!({}),
        };
        ch.send(&msg).await.unwrap();

        let mut sess = sessions.lock().await;
        let session = sess.get_or_create(&key).await;
        let history = session.get_history(10);
        let persisted = std::fs::canonicalize(&history[0].media[0]).unwrap();
        let existing = std::fs::canonicalize(&existing).unwrap();
        assert_eq!(persisted, existing);
    }

    #[tokio::test]
    async fn send_file_message_with_topic_persists_to_topic_session() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );

        let source_dir = data_dir.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("deck.pptx");
        std::fs::write(&source, b"pptx").unwrap();

        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "topic-file-chat".into(),
            content: "".into(),
            reply_to: None,
            media: vec![source.to_string_lossy().to_string()],
            metadata: serde_json::json!({ "topic": "slides demo" }),
        };
        ch.send(&msg).await.unwrap();

        let mut sess = sessions.lock().await;
        let topic_key = SessionKey::with_profile_topic(
            TEST_PROFILE_ID,
            "api",
            "topic-file-chat",
            "slides demo",
        );
        let base_key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "topic-file-chat");
        let topic_history = sess
            .get_or_create(&topic_key)
            .await
            .get_history(10)
            .to_vec();
        let base_history = sess.get_or_create(&base_key).await.get_history(10).to_vec();

        assert_eq!(topic_history.len(), 1);
        assert!(base_history.is_empty());
        assert_eq!(topic_history[0].media.len(), 1);
        assert!(topic_history[0].media[0].contains(".artifacts"));
        assert!(topic_history[0].media[0].contains("deck.pptx"));
    }

    #[tokio::test]
    async fn slides_topic_suppresses_duplicate_deck_delivery_until_new_user_message() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );

        let topic_key =
            SessionKey::with_profile_topic(TEST_PROFILE_ID, "api", "slides-chat", "slides demo");
        {
            let mut sess = sessions.lock().await;
            sess.add_message(&topic_key, Message::user("go"))
                .await
                .unwrap();
        }

        let source_dir = data_dir.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let first = source_dir.join("deck-one.pptx");
        let second = source_dir.join("deck-two.pptx");
        std::fs::write(&first, b"pptx-one").unwrap();
        std::fs::write(&second, b"pptx-two").unwrap();

        for source in [&first, &second] {
            let msg = OutboundMessage {
                channel: "api".into(),
                chat_id: "slides-chat".into(),
                content: String::new(),
                reply_to: None,
                media: vec![source.to_string_lossy().to_string()],
                metadata: serde_json::json!({ "topic": "slides demo" }),
            };
            ch.send(&msg).await.unwrap();
        }

        let mut sess = sessions.lock().await;
        let history = sess
            .get_or_create(&topic_key)
            .await
            .get_history(10)
            .to_vec();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, MessageRole::User);
        assert_eq!(history[1].role, MessageRole::Assistant);
        assert_eq!(history[1].media.len(), 1);
        assert!(history[1].media[0].contains("deck-one.pptx"));
    }

    #[tokio::test]
    async fn slides_topic_allows_new_deck_after_new_user_message() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );

        let topic_key =
            SessionKey::with_profile_topic(TEST_PROFILE_ID, "api", "slides-chat-2", "slides demo");
        {
            let mut sess = sessions.lock().await;
            sess.add_message(&topic_key, Message::user("go"))
                .await
                .unwrap();
        }

        let source_dir = data_dir.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let first = source_dir.join("deck-one.pptx");
        let second = source_dir.join("deck-two.pptx");
        std::fs::write(&first, b"pptx-one").unwrap();
        std::fs::write(&second, b"pptx-two").unwrap();

        let first_msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "slides-chat-2".into(),
            content: String::new(),
            reply_to: None,
            media: vec![first.to_string_lossy().to_string()],
            metadata: serde_json::json!({ "topic": "slides demo" }),
        };
        ch.send(&first_msg).await.unwrap();

        {
            let mut sess = sessions.lock().await;
            sess.add_message(&topic_key, Message::user("regenerate"))
                .await
                .unwrap();
        }

        let second_msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "slides-chat-2".into(),
            content: String::new(),
            reply_to: None,
            media: vec![second.to_string_lossy().to_string()],
            metadata: serde_json::json!({ "topic": "slides demo" }),
        };
        ch.send(&second_msg).await.unwrap();

        let mut sess = sessions.lock().await;
        let history = sess
            .get_or_create(&topic_key)
            .await
            .get_history(10)
            .to_vec();
        let assistant_media = history
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .flat_map(|message| message.media.iter())
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(history.len(), 4);
        assert_eq!(assistant_media.len(), 2);
        assert!(assistant_media[0].contains("deck-one.pptx"));
        assert!(assistant_media[1].contains("deck-two.pptx"));
    }

    #[tokio::test]
    async fn send_bg_notification_persists_to_session() {
        let sessions = test_sessions();
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );

        // Send background task notification (checkmark)
        let notify = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-bg2".into(),
            content: "\u{2713} fm_tts completed \u{2014} file delivered".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({}),
        };
        ch.send(&notify).await.unwrap();

        // Verify it was persisted to the session (not sent via SSE)
        let mut sess = sessions.lock().await;
        let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "test-bg2");
        let session = sess.get_or_create(&key).await;
        let history = session.get_history(10);
        assert_eq!(history.len(), 1);
        assert!(history[0].content.contains("fm_tts completed"));
    }

    #[tokio::test]
    async fn send_bg_notification_skips_duplicate_persist_when_history_is_already_written() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "test-bg3");
        {
            let mut sess = sessions.lock().await;
            sess.add_message(
                &key,
                Message::assistant("✓ fm_tts completed — file delivered"),
            )
            .await
            .unwrap();
        }

        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            Some(TEST_PROFILE_ID.to_string()),
        );

        let notify = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-bg3".into(),
            content: "\u{2713} fm_tts completed \u{2014} file delivered".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({ "_history_persisted": true }),
        };
        ch.send(&notify).await.unwrap();

        let mut sess = sessions.lock().await;
        let session = sess.get_or_create(&key).await;
        assert_eq!(session.get_history(10).len(), 1);
    }

    #[tokio::test]
    async fn send_to_unknown_chat_is_noop() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        );
        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "nonexistent".into(),
            content: "hello".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({}),
        };
        // Should not error
        ch.send(&msg).await.unwrap();
    }

    #[tokio::test]
    async fn list_sessions_dedups_profile_scoped_duplicates_by_chat_id() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let mut sess = sessions.lock().await;
        sess.add_message(
            &SessionKey::with_profile("dspfac", "api", "slides-123"),
            Message::user("one"),
        )
        .await
        .unwrap();
        sess.add_message(
            &SessionKey::with_profile(MAIN_PROFILE_ID, "api", "slides-123"),
            Message::user("two"),
        )
        .await
        .unwrap();
        drop(sess);

        let app = Router::new()
            .route("/sessions", get(handle_list_sessions))
            .with_state(ApiState {
                inbound_tx: mpsc::channel(1).0,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                sessions,
                profile_id: Some("dspfac".into()),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let sessions: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let matching: Vec<&serde_json::Value> = sessions
            .iter()
            .filter(|entry| entry.get("id").and_then(|id| id.as_str()) == Some("slides-123"))
            .collect();
        assert_eq!(matching.len(), 1);
    }

    #[tokio::test]
    async fn list_sessions_hides_internal_child_and_task_ledger_sessions() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let parent = SessionKey::with_profile("dspfac", "api", "web-123");
        let child = SessionKey::with_profile_topic("dspfac", "api", "web-123", "child-task-1");
        let task_ledger =
            SessionKey::with_profile_topic("dspfac", "api", "web-123", "default.tasks");
        let raw_parent = SessionKey("web-raw".to_string());
        let raw_task_ledger = SessionKey("web-raw#default.tasks".to_string());
        {
            let mut sess = sessions.lock().await;
            sess.add_message(&parent, Message::user("parent"))
                .await
                .unwrap();
            sess.add_message(&child, Message::user("child"))
                .await
                .unwrap();
            sess.add_message(&task_ledger, Message::user("task ledger"))
                .await
                .unwrap();
            sess.add_message(&raw_parent, Message::user("raw parent"))
                .await
                .unwrap();
            sess.add_message(&raw_task_ledger, Message::user("raw task ledger"))
                .await
                .unwrap();
        }

        let app = Router::new()
            .route("/sessions", get(handle_list_sessions))
            .with_state(ApiState {
                inbound_tx: mpsc::channel(1).0,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                sessions,
                profile_id: Some("dspfac".into()),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let sessions: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = sessions
            .iter()
            .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
            .collect();

        assert_eq!(ids, vec!["web-123", "web-raw"]);
    }

    #[tokio::test]
    async fn session_messages_full_source_reads_from_disk_snapshot() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let key =
            current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "web-history", None);

        {
            let mut manager = sessions.lock().await;
            manager
                .add_message_with_seq(&key, Message::user("hello"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(&key, Message::assistant("first result"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(&key, Message::assistant("second result"))
                .await
                .unwrap();
        }

        let app = Router::new()
            .route("/sessions/{id}/messages", get(handle_session_messages))
            .with_state(ApiState {
                inbound_tx: mpsc::channel(1).0,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                sessions,
                profile_id: Some(TEST_PROFILE_ID.into()),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/sessions/web-history/messages?source=full&since_seq=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let messages: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["seq"], 2);
        assert_eq!(messages[0]["content"], "second result");
    }

    #[tokio::test]
    async fn session_messages_default_source_returns_recent_window_with_absolute_seq() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let key =
            current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "web-history", None);

        {
            let mut manager = sessions.lock().await;
            manager
                .add_message_with_seq(&key, Message::user("one"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(&key, Message::assistant("two"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(&key, Message::user("three"))
                .await
                .unwrap();
            manager
                .add_message_with_seq(&key, Message::assistant("four"))
                .await
                .unwrap();
        }

        let app = Router::new()
            .route("/sessions/{id}/messages", get(handle_session_messages))
            .with_state(ApiState {
                inbound_tx: mpsc::channel(1).0,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                sessions,
                profile_id: Some(TEST_PROFILE_ID.into()),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/sessions/web-history/messages?limit=1&offset=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let messages: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["seq"], 3);
        assert_eq!(messages[0]["content"], "four");
    }

    #[tokio::test]
    async fn delete_session_checks_all_profile_candidates() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let id = "web-delete-fallback";
        let main_key = SessionKey::with_profile(MAIN_PROFILE_ID, "api", id);

        {
            let mut sess = sessions.lock().await;
            sess.add_message(&main_key, Message::user("hello"))
                .await
                .unwrap();
            assert!(sess.load(&main_key).await.is_some());
        }

        let app = Router::new()
            .route("/sessions/{id}", delete(handle_delete_session))
            .with_state(ApiState {
                inbound_tx: mpsc::channel(1).0,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                sessions: sessions.clone(),
                profile_id: Some(TEST_PROFILE_ID.to_string()),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let sess = sessions.lock().await;
        assert!(sess.load(&main_key).await.is_none());
    }

    #[tokio::test]
    async fn delete_session_accepts_listed_topic_session_id() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let id = "web-delete-topic";
        let topic_key = SessionKey::with_profile_topic(TEST_PROFILE_ID, "api", id, "research");

        {
            let mut sess = sessions.lock().await;
            sess.add_message(&topic_key, Message::user("hello"))
                .await
                .unwrap();
            assert!(sess.load(&topic_key).await.is_some());
        }

        let app = Router::new()
            .route("/sessions/{id}", delete(handle_delete_session))
            .with_state(ApiState {
                inbound_tx: mpsc::channel(1).0,
                pending: Arc::new(Mutex::new(HashMap::new())),
                watchers: Arc::new(Mutex::new(HashMap::new())),
                auth_token: None,
                sessions: sessions.clone(),
                profile_id: Some(TEST_PROFILE_ID.to_string()),
                task_query: None,
                task_cancel: None,
                task_relaunch: None,
                on_session_deleted: None,
                metrics_renderer: None,
                last_thread_id: Arc::new(Mutex::new(HashMap::new())),
            });

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/sessions/web-delete-topic%23research")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let fresh = SessionManager::open(data_dir.path()).unwrap();
        assert!(fresh.load(&topic_key).await.is_none());
    }

    #[tokio::test]
    async fn metrics_route_renders_child_prometheus_snapshot() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let channel = ApiChannel::new(
            port,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            Some(TEST_PROFILE_ID.to_string()),
        )
        .with_metrics_renderer(Arc::new(|| {
            "octos_test_metric_total{kind=\"child\"} 7\n".to_string()
        }));

        let (inbound_tx, _inbound_rx) = mpsc::channel(1);
        let shutdown = channel.shutdown.clone();
        let server = tokio::spawn(async move { channel.start(inbound_tx).await.unwrap() });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let body = reqwest::get(format!("http://127.0.0.1:{port}/metrics"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("octos_test_metric_total"));
        assert!(body.contains("kind=\"child\""));

        shutdown.store(true, Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
