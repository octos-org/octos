//! API channel — HTTP endpoint for web clients.
//!
//! Provides a `POST /chat` endpoint that accepts messages and returns SSE responses.
//! Used by octos-web to route through the gateway for adaptive routing, queue modes,
//! multi-provider failover, etc.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
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
use eyre::{Result, eyre};
use futures::stream::{self, StreamExt};
use metrics::counter;
use octos_core::{
    EventEnvelope, InboundMessage, MAIN_PROFILE_ID, Message, MessageRole, OutboundMessage,
    SessionKey, ThreadId, TurnContext,
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
type EventSeqStore = Arc<StdMutex<HashMap<String, u64>>>;

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
    event_seq: EventSeqStore,
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

#[derive(Clone)]
struct UiEventSink {
    event_seq: EventSeqStore,
}

impl UiEventSink {
    fn new(event_seq: EventSeqStore) -> Self {
        Self { event_seq }
    }

    fn encode(&self, ctx: &TurnContext, payload: serde_json::Value) -> Result<String> {
        let mut canonical_payload = payload;
        let Some(payload_obj) = canonical_payload.as_object_mut() else {
            return Err(eyre!("UI event payload must be a JSON object"));
        };

        let event_type = payload_obj
            .get("type")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| eyre!("UI event payload missing non-empty type"))?
            .to_string();
        let tool_call_id = payload_obj
            .get("tool_call_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        payload_obj.insert(
            "thread_id".to_string(),
            serde_json::Value::String(ctx.thread_id_str().to_string()),
        );

        let seq = self.next_seq(ctx)?;
        let envelope = EventEnvelope::new(
            ctx,
            seq,
            event_type.clone(),
            tool_call_id,
            canonical_payload.clone(),
        );
        let mut wire = serde_json::to_value(envelope)?;
        let wire_obj = wire
            .as_object_mut()
            .ok_or_else(|| eyre!("UI event envelope did not serialize to an object"))?;
        if let Some(payload_obj) = canonical_payload.as_object() {
            for (key, value) in payload_obj {
                wire_obj.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        wire_obj.insert(
            "type".to_string(),
            serde_json::Value::String(event_type.clone()),
        );
        wire_obj.insert(
            "thread_id".to_string(),
            serde_json::Value::String(ctx.thread_id_str().to_string()),
        );
        wire_obj.insert(
            "session_id".to_string(),
            serde_json::Value::String(ctx.session_id.clone()),
        );
        wire_obj.insert(
            "event_seq".to_string(),
            serde_json::Value::Number(seq.into()),
        );
        wire_obj.insert(
            "event_type".to_string(),
            serde_json::Value::String(event_type),
        );
        if let Some(topic) = ctx.topic.as_ref() {
            wire_obj.insert(
                "topic".to_string(),
                serde_json::Value::String(topic.clone()),
            );
        }

        Ok(wire.to_string())
    }

    fn next_seq(&self, ctx: &TurnContext) -> Result<u64> {
        let key = event_seq_key(ctx);
        let mut seqs = self
            .event_seq
            .lock()
            .map_err(|_| eyre!("UI event sequence store poisoned"))?;
        let next = seqs.entry(key).or_insert(0);
        *next += 1;
        Ok(*next)
    }
}

fn event_seq_key(ctx: &TurnContext) -> String {
    format!(
        "{}\u{1E}{}\u{1E}{}",
        ctx.session_id,
        ctx.topic.as_deref().unwrap_or_default(),
        ctx.thread_id_str()
    )
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
    /// Canonical per-turn routing id. During the one-release migration the
    /// legacy `client_message_id` request key is accepted as an alias, but
    /// business logic reads only this `thread_id` field.
    #[serde(default, alias = "client_message_id")]
    thread_id: Option<String>,
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
    /// Monotonic event sequence per `(session, topic, thread)` for the
    /// web/SSE ownership envelope.
    event_seq: EventSeqStore,
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
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
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

    fn ui_event_sink(&self) -> UiEventSink {
        UiEventSink::new(self.event_seq.clone())
    }

    async fn broadcast_session_event(&self, ctx: &TurnContext, event: serde_json::Value) {
        let payload = match self.ui_event_sink().encode(ctx, event) {
            Ok(payload) => payload,
            Err(error) => {
                warn!(
                    session_id = %ctx.session_id,
                    thread_id = %ctx.thread_id_str(),
                    %error,
                    "refusing to broadcast malformed UI event"
                );
                return;
            }
        };

        {
            let mut pending = self.pending.lock().await;
            if let Some(tx) = pending.get(&ctx.session_id) {
                if tx.send(payload.clone()).is_err() {
                    pending.remove(&ctx.session_id);
                }
            }
        }

        let mut watchers = self.watchers.lock().await;
        let key = watcher_key(&ctx.session_id, ctx.topic.as_deref());
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
/// iteration. The caller must provide the ingress-built turn context so
/// the first wire event is already server-stamped with ownership.
fn initial_sse_events(
    sink: &UiEventSink,
    ctx: &TurnContext,
    has_media: bool,
) -> Result<Vec<String>> {
    let thinking = serde_json::json!({
        "type": "thinking",
        "iteration": 0,
    });
    let mut events = vec![sink.encode(ctx, thinking)?];

    if has_media {
        let preprocessing = serde_json::json!({
            "type": "tool_progress",
            "tool": "preprocessing",
            "message": "Processing attachments...",
        });
        events.push(sink.encode(ctx, preprocessing)?);
    }

    Ok(events)
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
            event_seq: self.event_seq.clone(),
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
        // Every live SSE event below must be stamped from an explicit
        // thread_id carried by the caller. The previous per-chat sticky
        // fallback was last-writer-wins under rapid concurrent turns, so
        // missing metadata is now a server-side emission error instead of
        // an inferred route.
        let metadata_thread_id = outbound_thread_id(&msg.metadata);
        let turn_context = metadata_thread_id.as_deref().map(|tid| {
            TurnContext::new(
                msg.chat_id.clone(),
                topic
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                ThreadId::new(tid),
            )
        });

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
                // Route through the typed assistant constructor when the
                // outbound carries a thread_id so the persisted JSONL row is
                // pinned to the same turn as the live event envelope.
                // Preserve only the human-facing caption. The API/web path
                // already has structured media handles, so persisting
                // synthetic legacy `[file:...]` lines here creates duplicate
                // terminal file deliveries for the same artifact.
                let mut session_msg = match metadata_thread_id.as_deref() {
                    Some(tid) if !tid.is_empty() => {
                        Message::assistant_with_thread(msg.content.clone(), ThreadId::new(tid))
                    }
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
                    let ctx = turn_context.as_ref().ok_or_else(|| {
                        eyre!(
                            "refusing to emit session_result without required thread_id for session {}",
                            msg.chat_id
                        )
                    })?;
                    record_duplicate_result_suppressed(
                        "session_result_preferred_over_legacy_file_event",
                    );
                    self.broadcast_session_event(ctx, event).await;
                }
                return Ok(());
            }

            if let Some(message) = committed_message {
                record_result_delivery(
                    "session_result_event",
                    "committed_media_message",
                    "session_result",
                );
                let ctx = turn_context.as_ref().ok_or_else(|| {
                    eyre!(
                        "refusing to emit session_result without required thread_id for session {}",
                        msg.chat_id
                    )
                })?;
                record_duplicate_result_suppressed(
                    "committed_session_result_preferred_over_legacy_file_event",
                );
                self.broadcast_session_event(
                    ctx,
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
                    let event = serde_json::json!({
                        "type": "file",
                        "path": response_path_for_session_file(&data_dir, Path::new(persisted_path))
                            .unwrap_or_else(|| persisted_path.clone()),
                        "filename": filename,
                        "caption": msg.content,
                        "tool_call_id": tool_call_id,
                    });
                    let ctx = turn_context.as_ref().ok_or_else(|| {
                        eyre!(
                            "refusing to emit file without required thread_id for session {}",
                            msg.chat_id
                        )
                    })?;
                    let payload = self.ui_event_sink().encode(ctx, event)?;
                    let _ = tx.send(payload);
                }
            }
            return Ok(());
        }

        // Task status change — push raw JSON through SSE
        if let Some(task_json) = msg.metadata.get("_task_status").and_then(|v| v.as_str()) {
            let event = build_task_status_event(
                serde_json::from_str::<serde_json::Value>(task_json).unwrap_or_default(),
                topic,
            );
            let ctx = turn_context.as_ref().ok_or_else(|| {
                eyre!(
                    "refusing to emit task_status without required thread_id for session {}",
                    msg.chat_id
                )
            })?;
            self.broadcast_session_event(ctx, event).await;
            return Ok(());
        }

        if let Some(result) = session_result.as_ref() {
            let data_dir = {
                let sess = self.sessions.lock().await;
                sess.data_dir()
            };
            if let Some(event) = build_session_result_event(result, &data_dir, None, topic) {
                let ctx = turn_context.as_ref().ok_or_else(|| {
                    eyre!(
                        "refusing to emit session_result without required thread_id for session {}",
                        msg.chat_id
                    )
                })?;
                record_result_delivery("session_result_event", "metadata", "session_result");
                self.broadcast_session_event(ctx, event).await;
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
                let session_msg = match metadata_thread_id.as_deref() {
                    Some(tid) if !tid.is_empty() => {
                        Message::assistant_with_thread(msg.content.clone(), ThreadId::new(tid))
                    }
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
            let ctx = turn_context.as_ref().ok_or_else(|| {
                eyre!(
                    "refusing to emit live SSE event without required thread_id for session {}",
                    msg.chat_id
                )
            })?;
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
                        for event in build_bg_task_tool_start_events(&tasks) {
                            let payload = self.ui_event_sink().encode(ctx, event)?;
                            let _ = tx.send(payload);
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
                let payload = self.ui_event_sink().encode(ctx, done)?;
                let _ = tx.send(payload);
                pending.remove(&msg.chat_id);
                drop(pending);
                // Drop both the per-thread and the legacy chat-only entries
                // for this turn so subsequent turns start fresh. The
                // chat-only key is removed defensively — older code paths
                // may have written to it for events without thread_id.
                let mut last = self.last_content.lock().await;
                last.remove(&last_content_key(
                    &msg.chat_id,
                    metadata_thread_id.as_deref(),
                ));
                last.remove(&msg.chat_id);
            } else if !msg.content.is_empty() {
                // Regular message — send as replace event (full text replacement).
                let event = serde_json::json!({
                    "type": "replace",
                    "text": msg.content,
                });
                let payload = self.ui_event_sink().encode(ctx, event)?;
                if tx.send(payload).is_err() {
                    pending.remove(&msg.chat_id);
                }
            }
        }
        Ok(())
    }

    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        // Resolve the thread_id ahead of `send`/seeding so the per-thread
        // last_content key matches what `edit_message` uses on the next
        // chunk. Missing thread_id fail-closes before stream state or a
        // synthetic message id is created.
        let metadata_thread_id = outbound_thread_id(&msg.metadata).ok_or_else(|| {
            eyre!(
                "refusing to start live SSE stream without required thread_id for session {}",
                msg.chat_id
            )
        })?;
        // Reset delta tracking for this stream — keyed by (chat, thread)
        // so that turn A's reset never wipes turn B's prev-content under
        // concurrent overflow. The chat-only entry is also cleared so
        // legacy single-keyed state from before per-thread keying does
        // not leak across.
        {
            let mut last = self.last_content.lock().await;
            last.remove(&last_content_key(
                &msg.chat_id,
                Some(metadata_thread_id.as_str()),
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
        // Use the explicit thread_id so the seed lands under the same key
        // the next `edit_message` will read from — otherwise a chat-only
        // seed would force a wasteful first `replace` and re-introduce
        // cross-talk under concurrent overflow.
        if !msg.content.is_empty() {
            self.last_content.lock().await.insert(
                last_content_key(&msg.chat_id, Some(metadata_thread_id.as_str())),
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
        Ok(Some(encode_sse_message_id(
            &msg.chat_id,
            Some(metadata_thread_id.as_str()),
        )))
    }

    async fn edit_message(&self, chat_id: &str, message_id: &str, new_content: &str) -> Result<()> {
        // Forward to the bound implementation with no explicit override.
        // The synthetic message_id must carry the turn id; otherwise the
        // emission fails closed instead of guessing from session state.
        self.edit_message_bound(chat_id, message_id, new_content, None)
            .await
    }

    async fn edit_message_bound(
        &self,
        chat_id: &str,
        message_id: &str,
        new_content: &str,
        bound: Option<&str>,
    ) -> Result<()> {
        if new_content.is_empty() {
            return Ok(());
        }
        // M8.10 PR #2: recover the thread_id encoded into `send_with_id`'s
        // synthetic message_id so streaming `token`/`replace` events can be
        // demultiplexed by web clients running multiple in-flight threads
        // against the same chat_id.
        let (_, decoded_thread_id) = decode_sse_message_id(message_id);
        // Resolution priority is `bound > decoded`. The old sticky
        // per-chat fallback is intentionally gone: a missing thread_id is
        // an emission error, not something this layer may infer from the
        // latest turn on the same session.
        let bound_thread_id = bound.filter(|s| !s.is_empty());
        let thread_id = bound_thread_id.or(decoded_thread_id).ok_or_else(|| {
            eyre!(
                "refusing to edit live SSE message without required thread_id for session {chat_id}"
            )
        })?;
        let ctx = TurnContext::new(chat_id.to_string(), None, ThreadId::new(thread_id));
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
            let key = last_content_key(chat_id, Some(thread_id));
            let prev = last.get(&key).map(|s| s.as_str()).unwrap_or("");

            // If new content starts with the previous content, send only the delta.
            // This avoids re-rendering the entire message on each streaming update.
            if !prev.is_empty() && new_content.starts_with(prev) {
                let delta = &new_content[prev.len()..];
                if !delta.is_empty() {
                    let event = serde_json::json!({
                        "type": "token",
                        "text": delta,
                    });
                    let payload = self.ui_event_sink().encode(&ctx, event)?;
                    let _ = tx.send(payload);
                }
            } else {
                // Content changed non-incrementally (tool progress replaced, etc.)
                // Send full replacement.
                let event = serde_json::json!({
                    "type": "replace",
                    "text": new_content,
                });
                let payload = self.ui_event_sink().encode(&ctx, event)?;
                let _ = tx.send(payload);
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
        // Forward to the bound implementation with no explicit override.
        // The payload must carry a thread_id; otherwise the emission fails
        // closed. Non-API channels' default trait impl makes this a no-op.
        self.send_raw_sse_bound(chat_id, json, None).await
    }

    async fn send_raw_sse_bound(
        &self,
        chat_id: &str,
        json: &str,
        bound: Option<&str>,
    ) -> Result<()> {
        // Resolution priority is `bound > payload`. Missing thread_id is
        // an emission error: raw SSE payloads are no longer allowed to fall
        // through to a per-chat sticky route.
        let bound_thread_id = bound.filter(|s| !s.is_empty());
        let value = serde_json::from_str::<serde_json::Value>(json)?;
        let payload_thread_id = value
            .get("thread_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let thread_id = bound_thread_id.or(payload_thread_id).ok_or_else(|| {
            eyre!(
                "refusing to emit raw SSE payload without required thread_id for session {chat_id}"
            )
        })?;
        let topic = value
            .get("topic")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let ctx = TurnContext::new(chat_id.to_string(), topic, ThreadId::new(thread_id));
        let payload = self.ui_event_sink().encode(&ctx, value)?;
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

        // M9 event ownership: assistant/tool rows must arrive with their
        // owning thread stamped by the caller's TurnContext. Do not derive
        // from recent history and do not synthesize an orphan id here; either
        // would create a second persisted route that disagrees with live SSE.
        if message.thread_id.is_none()
            && matches!(
                message.role,
                octos_core::MessageRole::Assistant | octos_core::MessageRole::Tool
            )
        {
            tracing::warn!(
                chat_id = %chat_id,
                role = ?message.role,
                "persist_to_session: refusing unbound Assistant/Tool row; caller must pass TurnContext thread_id"
            );
            return None;
        }

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

    let request_topic = req
        .topic
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let Some(request_thread_id) = req
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
    else {
        return (StatusCode::BAD_REQUEST, "thread_id is required").into_response();
    };
    let turn_context = TurnContext::new(
        session_id.clone(),
        request_topic.clone(),
        ThreadId::new(request_thread_id.clone()),
    );
    let ui_sink = UiEventSink::new(state.event_seq.clone());

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
            let initial_events =
                match initial_sse_events(&ui_sink, &turn_context, !req.media.is_empty()) {
                    Ok(events) => events,
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("failed to build initial SSE events: {error}"),
                        )
                            .into_response();
                    }
                };
            for event in initial_events {
                let _ = tx.send(event);
            }
            pending.insert(session_id.clone(), tx);
            Some(rx)
        }
    };

    if !req.attach_only {
        // Build and send InboundMessage to the gateway bus.
        //
        // FA-12f/M9: thread the canonical `thread_id` through as the
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
                if let Some(topic) = request_topic.clone() {
                    metadata.insert("topic".to_string(), serde_json::Value::String(topic));
                }
                metadata.insert(
                    "thread_id".to_string(),
                    serde_json::Value::String(request_thread_id.clone()),
                );
                metadata.insert(
                    "client_message_id".to_string(),
                    serde_json::Value::String(request_thread_id.clone()),
                );
                serde_json::Value::Object(metadata)
            },
            message_id: Some(request_thread_id.clone()),
            origin: octos_core::MessageOrigin::ExternalUser,
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
    /// RFC3339 last-updated timestamp, for the resume picker's recency
    /// column. None for legacy files lacking the meta field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    /// The session's most recent user prompt (content-part text unwrapped,
    /// truncated) — the resume picker's preview. codex P2: the gateway path
    /// previously emitted only id/count/title, so gateway-only sessions had
    /// no preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_prompt: Option<String>,
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
        .list_top_level_sessions_with_meta()
        .into_iter()
        .filter_map(|(id, count, title, updated_at, last_prompt)| {
            let chat_id = api_chat_id_from_session_key(&id)?.to_string();
            if !seen.insert(chat_id.clone()) {
                return None;
            }
            Some(SessionInfo {
                id: chat_id,
                message_count: count,
                title,
                updated_at: updated_at.map(|ts| ts.to_rfc3339()),
                last_prompt,
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

    // #1377 (codex pre-merge P1): the download route resolves `up/` handles
    // against the PROCESS-GLOBAL upload root, so without an ownership check a
    // leaked/guessable `up/<other-tenant>/...` handle would let one tenant
    // download another's upload — the download-side twin of the upload-in gap
    // this PR closes.
    //
    // AUTHZ uses ONLY the server-trusted gateway tenant (`state.profile_id`,
    // fixed at startup), NEVER a request-supplied value. The gateway HTTP
    // layer has no per-request tenant authentication, so trusting a
    // `?target_profile_id` here would let an attacker name the foreign tenant
    // and pass the check (codex pre-merge P1 round-2). `_main` fallback keeps
    // a no-profile gateway gated (never None -> never skipped).
    //
    // Consequence: a cross-profile-ROUTED upload (stamped with a
    // `target_profile_id` other than the gateway profile) is NOT downloadable
    // through this raw `up/` route — the gateway cannot authenticate the
    // caller as that tenant, so it must refuse. The agent still reaches such a
    // file via the materialized workspace copy (`uploads/<name>`); only the
    // un-authenticatable raw-handle download is closed. Workspace/profile
    // files (not under the upload root) are unaffected.
    if crate::file_handle::is_under_upload_root(&canonical) {
        let tenant = state
            .profile_id
            .clone()
            .unwrap_or_else(|| octos_core::MAIN_PROFILE_ID.to_string());
        if !crate::file_handle::upload_owned_by_tenant(&canonical, Some(&tenant)) {
            return (StatusCode::FORBIDDEN, "access denied").into_response();
        }
    }

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

/// Query params for `POST /upload`. `target_profile_id` lets a cross-profile
/// routing client stamp the upload with the same tenant it will route the
/// subsequent `/chat` to (#1377 codex round-5 P2); absent for the common
/// single-profile case.
#[derive(Debug, Default, serde::Deserialize)]
struct UploadQuery {
    #[serde(default)]
    target_profile_id: Option<String>,
}

/// The effective OWNING tenant an `/upload` is STAMPED with (#1377): a
/// charset-guarded `?target_profile_id` (cross-profile routing), else the
/// gateway's own profile, else `_main`. NEVER `None`.
///
/// NOTE: this is the UPLOAD-STAMP tenant, used only for where to STORE the
/// file. It is NOT an authorization signal — `target_profile_id` is
/// request-supplied. The `/files` download gate authorizes against the
/// server-trusted `state.profile_id` ONLY (see `handle_file_download`).
fn effective_upload_tenant(
    target_profile_id: Option<&str>,
    gateway_profile_id: Option<&str>,
) -> String {
    target_profile_id
        .filter(|t| {
            !t.is_empty()
                && t.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        .map(str::to_string)
        .or_else(|| gateway_profile_id.map(str::to_string))
        .unwrap_or_else(|| octos_core::MAIN_PROFILE_ID.to_string())
}

/// POST /upload — upload files for use in chat media field.
///
/// #1377: stores uploads under `octos-uploads/<tenant>/<name>` where the
/// tenant is the gateway's resolved profile (the same layout the `octos serve`
/// handler uses), so the resolved `up/` handle carries the owning tenant and
/// the cross-tenant ownership gate (`upload_owned_by_tenant`) can enforce
/// isolation. A gateway with no profile (single-tenant / main) stores flat,
/// preserving the previous behaviour.
async fn handle_upload(
    State(state): State<ApiState>,
    axum::extract::Query(query): axum::extract::Query<UploadQuery>,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let upload_root = std::env::temp_dir().join("octos-uploads");
    // #1377: stamp the upload with the OWNING tenant so the resolved handle
    // matches what the subsequent `/chat` filters against. When the client
    // routes cross-profile it passes `?target_profile_id=<p>` (the same value
    // it sends on `/chat`); otherwise fall back to the gateway's own profile,
    // then to `_main` (codex round-7 P1: a gateway is a multi-tenant context,
    // so uploads are ALWAYS tenant-stamped — never flat — otherwise the
    // main/admin route would accept any tenant's handles). The target is
    // charset-guarded (lowercase alnum + `-`, the profile-id alphabet) so it
    // cannot inject a path component or `..`; an invalid/empty value falls
    // back to the gateway profile / `_main`.
    let tenant = effective_upload_tenant(
        query.target_profile_id.as_deref(),
        state.profile_id.as_deref(),
    );
    let upload_dir = upload_root.join(&tenant);
    // codex round-7 P2: a LEGACY flat upload named exactly like a profile id
    // (`octos-uploads/<tenant>`) would block `create_dir_all` and 500 every
    // upload for that tenant. Such a flat file is a pre-migration artifact in
    // the ephemeral temp dir that the new layout treats as un-owned (dropped
    // by the filter) anyway, so clear it to make room for the tenant dir.
    if tokio::fs::metadata(&upload_dir)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        warn!(
            tenant = %tenant,
            "removing legacy flat upload colliding with tenant directory"
        );
        let _ = tokio::fs::remove_file(&upload_dir).await;
    }
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
#[path = "api_channel_tests.rs"]
mod tests;
