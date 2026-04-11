//! API channel — HTTP endpoint for web clients.
//!
//! Provides a `POST /chat` endpoint that accepts messages and returns SSE responses.
//! Used by octos-web to route through the gateway for adaptive routing, queue modes,
//! multi-provider failover, etc.

use std::collections::HashMap;
use std::convert::Infallible;
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
use octos_core::{InboundMessage, Message, MessageRole, OutboundMessage, SessionKey};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use tracing::info;

use crate::SessionManager;
use crate::channel::Channel;

/// Callback that returns serialized task list for a session key.
pub type TaskQueryFn = dyn Fn(&str) -> serde_json::Value + Send + Sync;

/// Shared state for the API channel's HTTP handlers.
#[derive(Clone)]
struct ApiState {
    inbound_tx: mpsc::Sender<InboundMessage>,
    pending: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>>,
    auth_token: Option<String>,
    profile_id: String,
    sessions: Arc<Mutex<SessionManager>>,
    task_query: Option<Arc<TaskQueryFn>>,
}

/// Request body for POST /chat.
#[derive(Deserialize)]
struct ChatRequest {
    message: String,
    #[serde(default)]
    session_id: Option<String>,
    /// File paths from prior upload.
    #[serde(default)]
    media: Vec<String>,
    #[serde(default)]
    target_profile_id: Option<String>,
}

/// API channel that runs an HTTP server for web client access.
///
/// Messages flow: HTTP POST → InboundMessage → gateway bus → session actor →
/// OutboundMessage → `send()` → SSE events back to the HTTP response.
pub struct ApiChannel {
    port: u16,
    auth_token: Option<String>,
    profile_id: String,
    shutdown: Arc<AtomicBool>,
    pending: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>>,
    /// Track last sent content per chat_id for delta computation.
    last_content: Arc<Mutex<HashMap<String, String>>>,
    sessions: Arc<Mutex<SessionManager>>,
    /// Optional callback for querying background tasks by session key.
    task_query: Option<Arc<TaskQueryFn>>,
}

impl ApiChannel {
    pub fn new(
        port: u16,
        auth_token: Option<String>,
        shutdown: Arc<AtomicBool>,
        sessions: Arc<Mutex<SessionManager>>,
        profile_id: String,
    ) -> Self {
        Self {
            port,
            auth_token,
            profile_id,
            shutdown,
            pending: Arc::new(Mutex::new(HashMap::new())),
            last_content: Arc::new(Mutex::new(HashMap::new())),
            sessions,
            task_query: None,
        }
    }

    /// Attach a task query callback for the `/sessions/{id}/tasks` endpoint.
    pub fn with_task_query(mut self, f: Arc<TaskQueryFn>) -> Self {
        self.task_query = Some(f);
        self
    }
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
            auth_token: self.auth_token.clone(),
            profile_id: self.profile_id.clone(),
            sessions: self.sessions.clone(),
            task_query: self.task_query.clone(),
        };

        let app = Router::new()
            .route("/chat", post(handle_chat))
            .route("/sessions", get(handle_list_sessions))
            .route("/sessions/{id}/messages", get(handle_session_messages))
            .route("/sessions/{id}/status", get(handle_session_status))
            .route("/sessions/{id}/tasks", get(handle_session_tasks))
            .route("/sessions/{id}", delete(handle_delete_session))
            .route("/files/{*path}", get(handle_file_download))
            .route("/upload", post(handle_upload))
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
        if !msg.media.is_empty() {
            // File message — persist to session history AND send SSE event.
            let file_desc = msg
                .media
                .iter()
                .map(|p| {
                    let name = std::path::Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if msg.content.is_empty() {
                        format!("[file:{p}] {name}")
                    } else {
                        format!("[file:{p}] {name} — {}", msg.content)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let session_msg = Message {
                role: MessageRole::Assistant,
                content: file_desc,
                media: msg.media.clone(),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            };
            self.persist_to_session(&msg.chat_id, session_msg).await;

            // Send SSE file event so the web client receives it in real-time
            let pending = self.pending.lock().await;
            if let Some(tx) = pending.get(&msg.chat_id) {
                for path in &msg.media {
                    let filename = std::path::Path::new(path)
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
                        "path": path,
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
            let pending = self.pending.lock().await;
            if let Some(tx) = pending.get(&msg.chat_id) {
                let event = serde_json::json!({
                    "type": "task_status",
                    "task": serde_json::from_str::<serde_json::Value>(task_json).unwrap_or_default(),
                });
                let _ = tx.send(event.to_string());
            }
            return Ok(());
        }

        let is_bg_notification =
            msg.content.starts_with('\u{2713}') || msg.content.starts_with('\u{2717}');
        if is_bg_notification {
            // Background task notification — persist to session history.
            // Client polling will pick this up as the stop signal.
            let session_msg = Message {
                role: MessageRole::Assistant,
                content: msg.content.clone(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            };
            self.persist_to_session(&msg.chat_id, session_msg).await;
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

impl ApiChannel {
    /// Persist a message to the session JSONL for the given chat_id.
    async fn persist_to_session(&self, chat_id: &str, message: Message) {
        let key = SessionKey::with_profile(&self.profile_id, "api", chat_id);
        let mut sess = self.sessions.lock().await;
        if let Err(e) = sess.add_message(&key, message.clone()).await {
            tracing::warn!(chat_id = %chat_id, error = %e, "failed to persist message to session");
        } else {
            info!(chat_id = %chat_id, key = %key.0, "persisted file/notification to session");
        }

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
            pending.insert(session_id.clone(), tx);
            Some(rx)
        }
    };

    // Build and send InboundMessage to the gateway bus
    let inbound = InboundMessage {
        channel: "api".into(),
        sender_id: "web".into(),
        chat_id: session_id.clone(),
        content: req.message,
        timestamp: Utc::now(),
        media: req.media,
        metadata: match req.target_profile_id.filter(|value| !value.is_empty()) {
            Some(profile_id) => serde_json::json!({ "target_profile_id": profile_id }),
            None => serde_json::json!({}),
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

// ── Session REST endpoints ───────────────────────────────────────────

#[derive(Serialize)]
struct SessionInfo {
    id: String,
    message_count: usize,
}

#[derive(Serialize)]
struct MessageInfo {
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
}

fn default_limit() -> usize {
    100
}

/// GET /sessions/:id/status — check if a session has an active task.
async fn handle_session_status(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let pending = state.pending.lock().await;
    let active = pending.contains_key(&id);
    Json(serde_json::json!({
        "active": active,
    }))
    .into_response()
}

/// GET /sessions/:id/tasks — list background tasks for a session.
async fn handle_session_tasks(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let Some(ref query_fn) = state.task_query else {
        return Json(serde_json::json!([])).into_response();
    };
    let session_key = SessionKey::with_profile(&state.profile_id, "api", &id);
    let tasks = query_fn(&session_key.0);
    Json(tasks).into_response()
}

/// GET /sessions — list all API sessions.
async fn handle_list_sessions(State(state): State<ApiState>) -> Response {
    let sess = state.sessions.lock().await;
    let prefix = format!("{}:api:", state.profile_id);
    let list: Vec<SessionInfo> = sess
        .list_sessions()
        .into_iter()
        .filter_map(|(id, count)| {
            let chat_id = id.strip_prefix(&prefix)?;
            Some(SessionInfo {
                id: chat_id.to_string(),
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
    let key = SessionKey::with_profile(&state.profile_id, "api", &id);

    // source=full reads the append-only JSONL file (complete history).
    // Default reads from in-memory (may be compacted for LLM context).
    if params.source.as_deref() == Some("full") {
        let sess = state.sessions.lock().await;
        let flat_path = sess.session_path(&key);
        let data_dir = sess.data_dir();
        drop(sess);

        // Per-user session dir: data/users/{base_key}/sessions/
        // Contains default.jsonl + topic files like 123.jsonl
        // Read ALL jsonl files to merge messages across topics.
        let per_user_dir = {
            let base_key = key.base_key();
            let encoded = crate::session::encode_path_component(base_key);
            data_dir.join("users").join(encoded).join("sessions")
        };

        // Read from ALL per-user JSONL files + flat path and merge.
        let mut all_lines = Vec::new();

        // Primary: all per-user JSONL files (user messages + assistant responses)
        if let Ok(entries) = std::fs::read_dir(&per_user_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|x| x == "jsonl") {
                    if let Ok(content) = tokio::fs::read_to_string(entry.path()).await {
                        all_lines.extend(content.lines().map(|l| l.to_string()));
                    }
                }
            }
        }
        // Secondary: flat path (has file deliveries that may not be in per-user)
        if let Ok(content) = tokio::fs::read_to_string(&flat_path).await {
            // Only add lines from flat that have media (file deliveries) or bg notifications
            // and aren't already in per-user
            for line in content.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    let has_media = v
                        .get("media")
                        .and_then(|m| m.as_array())
                        .is_some_and(|a| !a.is_empty());
                    let is_bg = v
                        .get("content")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| c.starts_with('✓') || c.starts_with('✗'));
                    if has_media || is_bg {
                        // Check for duplicate by content
                        let content_str = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        if !all_lines.iter().any(|existing| {
                            existing.contains(content_str) && !content_str.is_empty()
                        }) {
                            all_lines.push(line.to_string());
                        }
                    }
                }
            }
        }
        let messages: Vec<MessageInfo> = all_lines
            .iter()
            .filter_map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).ok()?;
                let role = v.get("role")?.as_str()?;
                let content = v.get("content")?.as_str().unwrap_or("");
                let timestamp = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("");
                let media: Vec<String> = v
                    .get("media")
                    .and_then(|m| m.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let tool_calls: Vec<serde_json::Value> = v
                    .get("tool_calls")
                    .and_then(|tc| tc.as_array())
                    .map(|arr| arr.to_vec())
                    .unwrap_or_default();
                Some(MessageInfo {
                    role: role.to_string(),
                    content: content.to_string(),
                    timestamp: timestamp.to_string(),
                    media,
                    tool_calls,
                })
            })
            .skip(offset)
            .take(limit)
            .collect();
        if !messages.is_empty() {
            return Json(messages).into_response();
        }
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    }

    let mut sess = state.sessions.lock().await;
    let session = sess.get_or_create(&key).await;
    let history = session.get_history(fetch_count).to_vec();
    let messages: Vec<MessageInfo> = history
        .iter()
        .skip(offset)
        .take(limit)
        .map(|m| MessageInfo {
            role: m.role.to_string(),
            content: m.content.clone(),
            timestamp: m.timestamp.to_rfc3339(),
            media: m.media.clone(),
            tool_calls: m
                .tool_calls
                .as_ref()
                .map(|tcs| {
                    tcs.iter()
                        .filter_map(|tc| serde_json::to_value(tc).ok())
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    Json(messages).into_response()
}

/// DELETE /sessions/:id — delete a session.
async fn handle_delete_session(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let key = SessionKey::with_profile(&state.profile_id, "api", &id);
    let mut sess = state.sessions.lock().await;
    match sess.clear(&key).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// GET /files/*path — download a file produced by write_file/send_file.
async fn handle_file_download(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    let file_path = std::path::Path::new(&path);

    // Security: only serve files from known safe directories
    let canonical = match std::fs::canonicalize(file_path) {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "file not found").into_response(),
    };

    // Must be under $HOME/.octos or /tmp (NOT the entire $HOME)
    let home = std::env::var("HOME").unwrap_or_default();
    let octos_dir = std::fs::canonicalize(format!("{home}/.octos"))
        .unwrap_or_else(|_| std::path::PathBuf::from(format!("{home}/.octos")));
    let tmp_dir =
        std::fs::canonicalize("/tmp").unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    let allowed = canonical.starts_with(&octos_dir) || canonical.starts_with(&tmp_dir);
    if !allowed {
        return (StatusCode::FORBIDDEN, "access denied").into_response();
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
        paths.push(dest.to_string_lossy().to_string());
    }

    Json(paths).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PROFILE_ID: &str = "dspfac";

    fn test_sessions() -> Arc<Mutex<SessionManager>> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(Mutex::new(SessionManager::open(dir.path()).unwrap()))
    }

    #[test]
    fn chat_request_deserialize() {
        let json = r#"{"message": "hello"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "hello");
        assert!(req.session_id.is_none());
    }

    #[test]
    fn chat_request_with_session() {
        let json = r#"{"message": "hi", "session_id": "web-123"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.session_id.as_deref(), Some("web-123"));
    }

    #[test]
    fn api_channel_name() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            TEST_PROFILE_ID.to_string(),
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
            TEST_PROFILE_ID.to_string(),
        );
        assert_eq!(ch.max_message_length(), 1_000_000);
    }

    #[tokio::test]
    async fn send_to_pending_client() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            TEST_PROFILE_ID.to_string(),
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
    async fn send_completion_closes_stream() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            TEST_PROFILE_ID.to_string(),
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
            metadata: serde_json::json!({"_completion": true}),
        };
        ch.send(&msg).await.unwrap();

        // Should receive done event
        let event = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(parsed["type"], "done");

        // Sender was removed — next recv returns None
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn send_completion_with_bg_tasks_closes_and_client_polls() {
        let sessions = test_sessions();
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            TEST_PROFILE_ID.to_string(),
        );
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
            media: vec!["/tmp/test.mp3".into()],
            metadata: serde_json::json!({}),
        };
        ch.send(&file_msg).await.unwrap();

        // Client polling session history would find it
        let mut sess = sessions.lock().await;
        let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "test-bg");
        let session = sess.get_or_create(&key).await;
        let history = session.get_history(10);
        assert!(
            history
                .iter()
                .any(|m| m.media.contains(&"/tmp/test.mp3".to_string()))
        );
    }

    #[tokio::test]
    async fn send_file_message_persists_to_session() {
        let sessions = test_sessions();
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            TEST_PROFILE_ID.to_string(),
        );

        // Send a file message (no active SSE needed — goes straight to session)
        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "test-file".into(),
            content: "Audio file".into(),
            reply_to: None,
            media: vec!["/tmp/test.mp3".into()],
            metadata: serde_json::json!({}),
        };
        ch.send(&msg).await.unwrap();

        // Verify it was persisted to the session
        let mut sess = sessions.lock().await;
        let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "test-file");
        let session = sess.get_or_create(&key).await;
        let history = session.get_history(10);
        assert_eq!(history.len(), 1);
        assert!(history[0].content.contains("/tmp/test.mp3"));
        assert!(history[0].content.contains("Audio file"));
        assert_eq!(history[0].media, vec!["/tmp/test.mp3".to_string()]);
    }

    #[tokio::test]
    async fn send_bg_notification_persists_to_session() {
        let sessions = test_sessions();
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            sessions.clone(),
            TEST_PROFILE_ID.to_string(),
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
    async fn send_to_unknown_chat_is_noop() {
        let ch = ApiChannel::new(
            8091,
            None,
            Arc::new(AtomicBool::new(false)),
            test_sessions(),
            TEST_PROFILE_ID.to_string(),
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
}
