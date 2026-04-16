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
use axum::routing::{delete, get, post};
use chrono::Utc;
use eyre::Result;
use futures::stream::{self, StreamExt};
use metrics::counter;
use octos_core::{
    InboundMessage, MAIN_PROFILE_ID, Message, MessageRole, OutboundMessage, SessionKey,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use crate::SessionManager;
use crate::channel::Channel;
use crate::file_handle::{
    encode_profile_file_handle, resolve_legacy_file_request, resolve_scoped_file_handle,
};

/// Callback that returns serialized task list for a session key.
pub type TaskQueryFn = dyn Fn(&str) -> serde_json::Value + Send + Sync;

/// Callback invoked when a session is deleted via the API.
/// The gateway runtime wires this to stop the session actor.
type OnSessionDeletedFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Shared state for the API channel's HTTP handlers.
#[derive(Clone)]
struct ApiState {
    inbound_tx: mpsc::Sender<InboundMessage>,
    pending: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>>,
    watchers: Arc<Mutex<HashMap<String, Vec<mpsc::UnboundedSender<String>>>>>,
    auth_token: Option<String>,
    profile_id: Option<String>,
    sessions: Arc<Mutex<SessionManager>>,
    task_query: Option<Arc<TaskQueryFn>>,
    on_session_deleted: Option<OnSessionDeletedFn>,
    metrics_renderer: Option<Arc<dyn Fn() -> String + Send + Sync>>,
}

fn watcher_key(chat_id: &str, topic: Option<&str>) -> String {
    match topic.filter(|value| !value.trim().is_empty()) {
        Some(topic) => format!("{chat_id}::{}", topic.trim()),
        None => chat_id.to_string(),
    }
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
    pending: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>>,
    watchers: Arc<Mutex<HashMap<String, Vec<mpsc::UnboundedSender<String>>>>>,
    /// Track last sent content per chat_id for delta computation.
    last_content: Arc<Mutex<HashMap<String, String>>>,
    sessions: Arc<Mutex<SessionManager>>,
    /// Optional callback for querying background tasks by session key.
    task_query: Option<Arc<TaskQueryFn>>,
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
            sessions,
            task_query: None,
            on_session_deleted: None,
            metrics_renderer: None,
        }
    }

    /// Attach a task query callback for the `/sessions/{id}/tasks` endpoint.
    pub fn with_task_query(mut self, f: Arc<TaskQueryFn>) -> Self {
        self.task_query = Some(f);
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

    async fn materialize_media_for_session(&self, chat_id: &str, media: &[String]) -> Vec<String> {
        let key = current_profile_api_session_key(self.profile_id.as_deref(), chat_id);
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
        if let Some(subscribers) = watchers.get_mut(&key) {
            subscribers.retain(|tx| tx.send(payload.clone()).is_ok());
            if subscribers.is_empty() {
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

    if let Some(paths) = materialized_media {
        let response_media: Vec<String> = paths
            .iter()
            .map(|path| {
                response_path_for_session_file(data_dir, Path::new(path))
                    .unwrap_or_else(|| path.clone())
            })
            .collect();
        obj.insert("media".to_string(), serde_json::json!(response_media));
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

fn build_replay_complete_event(topic: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "type": "replay_complete",
        "topic": topic,
    })
}

fn initial_sse_events(has_media: bool) -> Vec<String> {
    let mut events = vec![
        serde_json::json!({
            "type": "thinking",
            "iteration": 0,
        })
        .to_string(),
    ];

    if has_media {
        events.push(
            serde_json::json!({
                "type": "tool_progress",
                "tool": "preprocessing",
                "message": "Processing attachments...",
            })
            .to_string(),
        );
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
            on_session_deleted: self.on_session_deleted.clone(),
            metrics_renderer: self.metrics_renderer.clone(),
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

        if !msg.media.is_empty() {
            let persisted_media = self
                .materialize_media_for_session(&msg.chat_id, &msg.media)
                .await;
            let data_dir = {
                let sess = self.sessions.lock().await;
                sess.data_dir()
            };

            // File message — persist to session history AND send SSE event.
            let file_desc = msg
                .media
                .iter()
                .zip(persisted_media.iter())
                .map(|(original_path, persisted_path)| {
                    let name = std::path::Path::new(original_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let handle =
                        response_path_for_session_file(&data_dir, Path::new(persisted_path))
                            .unwrap_or_else(|| persisted_path.clone());
                    if msg.content.is_empty() {
                        format!("[file:{handle}] {name}")
                    } else {
                        format!("[file:{handle}] {name} — {}", msg.content)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let committed_message = if !history_already_persisted {
                let session_msg = Message {
                    role: MessageRole::Assistant,
                    content: file_desc,
                    media: persisted_media.clone(),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    timestamp: chrono::Utc::now(),
                };
                self.persist_to_session(&msg.chat_id, session_msg).await
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
                    let event = serde_json::json!({
                        "type": "file",
                        "path": response_path_for_session_file(&data_dir, Path::new(persisted_path))
                            .unwrap_or_else(|| persisted_path.clone()),
                        "filename": filename,
                        "caption": msg.content,
                        "tool_call_id": tool_call_id,
                    });
                    let _ = tx.send(event.to_string());
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
            self.broadcast_session_event(&msg.chat_id, topic, event)
                .await;
            return Ok(());
        }

        if let Some(result) = session_result.as_ref() {
            let data_dir = {
                let sess = self.sessions.lock().await;
                sess.data_dir()
            };
            if let Some(event) = build_session_result_event(result, &data_dir, None, topic) {
                record_result_delivery("session_result_event", "metadata", "session_result");
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
            if !history_already_persisted {
                let session_msg = Message {
                    role: MessageRole::Assistant,
                    content: msg.content.clone(),
                    media: vec![],
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    timestamp: chrono::Utc::now(),
                };
                let _ = self.persist_to_session(&msg.chat_id, session_msg).await;
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
                let done = serde_json::json!({
                    "type": "done",
                    "content": "",
                    "model": msg.metadata.get("model").and_then(|v| v.as_str()).unwrap_or(""),
                    "provider": msg.metadata.get("provider").cloned().unwrap_or(serde_json::Value::Null),
                    "model_id": msg.metadata.get("model_id").cloned().unwrap_or(serde_json::Value::Null),
                    "endpoint": msg.metadata.get("endpoint").cloned().unwrap_or(serde_json::Value::Null),
                    "tokens_in": msg.metadata.get("tokens_in").and_then(|v| v.as_u64()).unwrap_or(0),
                    "tokens_out": msg.metadata.get("tokens_out").and_then(|v| v.as_u64()).unwrap_or(0),
                    "duration_s": msg.metadata.get("duration_s").and_then(|v| v.as_u64()).unwrap_or(0),
                    "has_bg_tasks": has_bg,
                });
                let _ = tx.send(done.to_string());
                pending.remove(&msg.chat_id);
                drop(pending);
                self.last_content.lock().await.remove(&msg.chat_id);
            } else if !msg.content.is_empty() {
                // Regular message — send as replace event (full text replacement).
                let event = serde_json::json!({
                    "type": "replace",
                    "text": msg.content,
                });
                if tx.send(event.to_string()).is_err() {
                    pending.remove(&msg.chat_id);
                }
            }
        }
        Ok(())
    }

    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        // Reset delta tracking — new message stream starts fresh
        self.last_content.lock().await.remove(&msg.chat_id);
        self.send(msg).await?;
        // Return a dummy ID so the stream forwarder uses edit_message() for
        // subsequent updates instead of calling send_with_id() again.
        Ok(Some(format!("sse-{}", msg.chat_id)))
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        _message_id: &str,
        new_content: &str,
    ) -> Result<()> {
        if new_content.is_empty() {
            return Ok(());
        }
        let pending = self.pending.lock().await;
        if let Some(tx) = pending.get(chat_id) {
            let mut last = self.last_content.lock().await;
            let prev = last.get(chat_id).map(|s| s.as_str()).unwrap_or("");

            // If new content starts with the previous content, send only the delta.
            // This avoids re-rendering the entire message on each streaming update.
            if !prev.is_empty() && new_content.starts_with(prev) {
                let delta = &new_content[prev.len()..];
                if !delta.is_empty() {
                    let event = serde_json::json!({
                        "type": "token",
                        "text": delta,
                    });
                    let _ = tx.send(event.to_string());
                }
            } else {
                // Content changed non-incrementally (tool progress replaced, etc.)
                // Send full replacement.
                let event = serde_json::json!({
                    "type": "replace",
                    "text": new_content,
                });
                let _ = tx.send(event.to_string());
            }
            last.insert(chat_id.to_string(), new_content.to_string());
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
        let pending = self.pending.lock().await;
        if let Some(tx) = pending.get(chat_id) {
            let _ = tx.send(json.to_string());
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
    /// Persist a message to the session JSONL for the given chat_id and
    /// return the authoritative committed message shape when available.
    async fn persist_to_session(&self, chat_id: &str, message: Message) -> Option<MessageInfo> {
        let key = current_profile_api_session_key(self.profile_id.as_deref(), chat_id);
        let mut sess = self.sessions.lock().await;
        let committed = match sess.add_message_with_seq(&key, message.clone()).await {
            Ok(seq) => {
                info!(chat_id = %chat_id, key = %key.0, seq, "persisted file/notification to session");
                Some(message_info_from_history_message(
                    &message,
                    &sess.data_dir(),
                    seq,
                ))
            }
            Err(e) => {
                tracing::warn!(chat_id = %chat_id, error = %e, "failed to persist message to session");
                None
            }
        };

        // Also write to the per-user SessionHandle path so the web client
        // (which reads from per-user JSONL via source=full) can see file deliveries.
        let data_dir = sess.data_dir();
        drop(sess);
        let base_key = key.base_key();
        let encoded = crate::session::encode_path_component(base_key);
        let per_user_dir = data_dir.join("users").join(encoded).join("sessions");
        let per_user_path = per_user_dir.join("default.jsonl");
        if per_user_path.exists() {
            if let Ok(msg_json) = serde_json::to_string(&message) {
                let path_clone = per_user_path.clone();
                match tokio::task::spawn_blocking(move || {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&per_user_path)?;
                    writeln!(f, "{}", msg_json)?;
                    Ok::<_, std::io::Error>(())
                })
                .await
                {
                    Ok(Ok(())) => info!(chat_id = %chat_id, "persisted to per-user session"),
                    Ok(Err(e)) => {
                        tracing::warn!(chat_id = %chat_id, path = %path_clone.display(), error = %e, "per-user session write failed")
                    }
                    Err(e) => {
                        tracing::warn!(chat_id = %chat_id, error = %e, "per-user session spawn_blocking failed")
                    }
                }
            }
        } else {
            tracing::debug!(chat_id = %chat_id, path = %per_user_path.display(), "per-user session path not found, skipping");
        }

        committed
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

    // Create per-request SSE channel. If a previous request is still streaming
    // AND alive, reuse it. Otherwise, replace the stale sender.
    let rx = {
        let mut pending = state.pending.lock().await;
        let stale = if let Some(old_tx) = pending.get(&session_id) {
            // Test if the receiver is still alive by sending a keepalive
            old_tx
                .send(serde_json::json!({"type":"keepalive"}).to_string())
                .is_err()
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
            let (tx, rx) = mpsc::unbounded_channel::<String>();
            for event in initial_sse_events(!req.media.is_empty()) {
                let _ = tx.send(event);
            }
            pending.insert(session_id.clone(), tx);
            Some(rx)
        }
    };

    if !req.attach_only {
        // Build and send InboundMessage to the gateway bus.
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
                serde_json::Value::Object(metadata)
            },
            message_id: None,
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

    // Return SSE stream that forwards events from the unbounded receiver
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(data) => {
                let event: Result<Event, Infallible> = Ok(Event::default().data(data));
                Some((event, rx))
            }
            None => None, // Channel closed (sender dropped) → stream ends
        }
    });

    Sse::new(stream)
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

    let (tx, rx) = mpsc::unbounded_channel::<String>();
    {
        let mut watchers = state.watchers.lock().await;
        watchers
            .entry(watcher_key(&id, params.topic.as_deref()))
            .or_default()
            .push(tx);
    }

    let mut replay_events = replay_task_status_events(&state, &id, params.topic.as_deref()).await;
    replay_events.extend(
        replay_committed_session_results(&state, &id, params.since_seq, params.topic.as_deref())
            .await,
    );
    replay_events.push(build_replay_complete_event(params.topic.as_deref()).to_string());
    record_replay("stream", "opened", 1);

    let live_stream = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(data) => {
                let event: Result<Event, Infallible> = Ok(Event::default().data(data));
                Some((event, rx))
            }
            None => None,
        }
    });

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
}

#[derive(Serialize)]
struct MessageInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seq: Option<usize>,
    role: String,
    content: String,
    timestamp: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    media: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<serde_json::Value>,
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

fn current_profile_api_session_key(profile_id: Option<&str>, chat_id: &str) -> SessionKey {
    current_profile_api_session_key_with_topic(profile_id, chat_id, None)
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
    let mut keys = Vec::with_capacity(3);

    if let Some(topic) = topic.filter(|value| !value.is_empty()) {
        if let Some(profile_id) = profile_id.filter(|value| !value.is_empty()) {
            keys.push(SessionKey::with_profile_topic(profile_id, "api", id, topic));
        }
        keys.push(SessionKey::with_profile_topic(
            MAIN_PROFILE_ID,
            "api",
            id,
            topic,
        ));
        keys.push(SessionKey::with_topic("api", id, topic));
    } else {
        if let Some(profile_id) = profile_id.filter(|value| !value.is_empty()) {
            keys.push(SessionKey::with_profile(profile_id, "api", id));
        }
        keys.push(SessionKey::with_profile(MAIN_PROFILE_ID, "api", id));
        keys.push(SessionKey::new("api", id));
    }

    keys.dedup_by(|left, right| left.0 == right.0);
    keys
}

fn api_chat_id_from_session_key(id: &str) -> Option<&str> {
    id.strip_prefix("api:")
        .or_else(|| id.split_once(":api:").map(|(_, chat_id)| chat_id))
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
        media: message
            .media
            .iter()
            .filter_map(|path| response_path_for_session_file(data_dir, Path::new(path)))
            .collect(),
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

async fn replay_task_status_events(state: &ApiState, id: &str, topic: Option<&str>) -> Vec<String> {
    let Some(ref query_fn) = state.task_query else {
        record_replay("task_status", "disabled", 1);
        return Vec::new();
    };

    let session_key =
        current_profile_api_session_key_with_topic(state.profile_id.as_deref(), id, topic);
    let events: Vec<String> = query_fn(&session_key.0)
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
    let sess = state.sessions.lock().await;
    let data_dir = sess.data_dir();

    for candidate in &candidates {
        if let Some(session) = sess.load(candidate).await {
            let events: Vec<String> = session
                .messages
                .iter()
                .enumerate()
                .filter(|(seq, message)| {
                    since_seq.is_none_or(|since| *seq > since)
                        && message.role == MessageRole::Assistant
                })
                .map(|(seq, message)| {
                    build_session_result_event_from_message(
                        message_info_from_history_message(message, &data_dir, seq),
                        topic,
                    )
                    .to_string()
                })
                .collect();
            if events.is_empty() {
                record_replay("session_result", "empty", 1);
            } else {
                record_replay("session_result", "emitted", events.len());
            }
            return events;
        }
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
        let session_key = current_profile_api_session_key_with_topic(
            state.profile_id.as_deref(),
            &id,
            params.topic.as_deref(),
        );
        task_list_has_active_tasks(&query_fn(&session_key.0))
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
    let session_key = current_profile_api_session_key_with_topic(
        state.profile_id.as_deref(),
        &id,
        params.topic.as_deref(),
    );
    let tasks = query_fn(&session_key.0);
    Json(tasks).into_response()
}

/// GET /sessions — list all API sessions.
async fn handle_list_sessions(State(state): State<ApiState>) -> Response {
    let sess = state.sessions.lock().await;
    let mut seen = std::collections::HashSet::new();
    let list: Vec<SessionInfo> = sess
        .list_sessions()
        .into_iter()
        .filter_map(|(id, count)| {
            let chat_id = api_chat_id_from_session_key(&id)?.to_string();
            if !seen.insert(chat_id.clone()) {
                return None;
            }
            Some(SessionInfo {
                id: chat_id,
                message_count: count,
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

    // source=full reads the append-only JSONL file (complete history).
    // Default reads from in-memory (may be compacted for LLM context).
    if params.source.as_deref() == Some("full") {
        let sess = state.sessions.lock().await;
        let data_dir = sess.data_dir();
        for candidate in &candidates {
            if let Some(session) = sess.load(candidate).await {
                let messages: Vec<MessageInfo> = session
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(seq, _)| params.since_seq.is_none_or(|since| *seq > since))
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

    let sess = state.sessions.lock().await;
    let data_dir = sess.data_dir();
    for candidate in &candidates {
        if let Some(session) = sess.load(candidate).await {
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
    let mut cleared = false;
    for candidate in api_session_key_candidates(state.profile_id.as_deref(), &id, None) {
        if sess.clear(&candidate).await.is_ok() {
            cleared = true;
            // Notify the gateway runtime to stop the session actor so it doesn't
            // serve stale context if new messages arrive for this session ID.
            if let Some(ref cb) = state.on_session_deleted {
                cb(&id);
            }
            return StatusCode::NO_CONTENT.into_response();
        }
    }
    if cleared {
        StatusCode::NO_CONTENT.into_response()
    } else {
        // No session found — still return 204 (idempotent delete)
        StatusCode::NO_CONTENT.into_response()
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

    fn test_sessions_in(data_dir: &Path) -> Arc<Mutex<SessionManager>> {
        Arc::new(Mutex::new(SessionManager::open(data_dir).unwrap()))
    }

    fn test_sessions() -> Arc<Mutex<SessionManager>> {
        let dir = std::env::temp_dir().join(format!("octos-bus-tests-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        test_sessions_in(&dir)
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
                on_session_deleted: None,
                metrics_renderer: None,
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
                on_session_deleted: None,
                metrics_renderer: None,
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
    fn api_session_key_candidates_prefer_current_profile() {
        let keys = api_session_key_candidates(Some("dspfac--newsbot"), "web-123", None);

        assert_eq!(keys[0].0, "dspfac--newsbot:api:web-123");
        assert_eq!(keys[1].0, "_main:api:web-123");
        assert_eq!(keys[2].0, "api:web-123");
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
        let events = initial_sse_events(false);
        assert_eq!(events.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(parsed["type"], "thinking");
        assert_eq!(parsed["iteration"], 0);
    }

    #[test]
    fn initial_sse_events_include_preprocessing_for_media() {
        let events = initial_sse_events(true);
        assert_eq!(events.len(), 2);
        let parsed: Vec<serde_json::Value> = events
            .iter()
            .map(|event| serde_json::from_str(event).unwrap())
            .collect();
        assert_eq!(parsed[0]["type"], "thinking");
        assert_eq!(parsed[1]["type"], "tool_progress");
        assert_eq!(parsed[1]["tool"], "preprocessing");
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
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
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
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
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

    #[tokio::test]
    async fn replay_committed_session_results_replays_only_newer_assistant_messages() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = test_sessions_in(data_dir.path());
        let key = current_profile_api_session_key(Some(TEST_PROFILE_ID), "test-chat");

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

        let state = ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions,
            task_query: None,
            on_session_deleted: None,
            metrics_renderer: None,
        };

        let replayed = replay_committed_session_results(&state, "test-chat", Some(1), None).await;

        assert_eq!(replayed.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&replayed[0]).unwrap();
        assert_eq!(parsed["type"], "session_result");
        assert_eq!(parsed["message"]["seq"], 2);
        assert_eq!(parsed["message"]["role"], "assistant");
        assert_eq!(parsed["message"]["content"], "second result");
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

        let state = ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions,
            task_query: None,
            on_session_deleted: None,
            metrics_renderer: None,
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
        assert_eq!(second["message"]["seq"], 2);
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
            on_session_deleted: None,
            metrics_renderer: None,
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
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
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

        // Sender was removed — next recv returns None
        assert!(rx.recv().await.is_none());
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
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
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
        assert!(rx.recv().await.is_none());

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
        assert!(history[0].content.contains("[file:pf/"));
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
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
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
            metadata: serde_json::json!({}),
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
                on_session_deleted: None,
                metrics_renderer: None,
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
