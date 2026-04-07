//! API request handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::StreamExt;
use octos_agent::Agent;
use octos_core::{AgentId, Message, SessionKey};
use serde::{Deserialize, Serialize};

use axum::http::HeaderMap;

use super::AppState;
use super::metrics::MetricsReporter;
use super::sse::ChannelReporter;

/// POST /api/chat -- send a message, get a response.
/// When `stream: true`, returns SSE events. Otherwise returns JSON.
#[derive(Deserialize)]
pub struct ChatRequest {
    pub message: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub stream: bool,
    /// File paths from prior `/api/upload` call.
    #[serde(default)]
    pub media: Vec<String>,
}

#[derive(Serialize)]
pub struct ChatResponse {
    pub content: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Maximum message length (1MB).
const MAX_MESSAGE_LEN: usize = 1_048_576;

/// Resolve API port for a specific profile, or fall back to first available.
/// Profile is identified by X-Profile-Id header (set by Caddy from subdomain).
async fn resolve_api_port(state: &AppState, headers: &HeaderMap) -> Option<(String, u16)> {
    let pm = state.process_manager.as_ref()?;

    // Check X-Profile-Id header first (set by reverse proxy from subdomain)
    if let Some(profile_id) = headers
        .get("x-profile-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        if let Some(port) = pm.api_port(profile_id).await {
            return Some((profile_id.to_string(), port));
        }
        tracing::warn!(profile = profile_id, "no API port for requested profile");
    }

    // Fall back to first available
    pm.first_api_port().await
}

pub async fn chat(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Response {
    // If a gateway has an API channel running, proxy the request to it.
    // The gateway's stream forwarder now sends discrete SSE events (thinking,
    // tool_start, tool_progress, cost_update) via send_raw_sse alongside
    // the text-based streaming updates.
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        return super::webhook_proxy::api_chat_proxy(
            &state,
            port,
            &req.message,
            req.session_id.as_deref(),
            &req.media,
        )
        .await;
    }

    // No gateway with API channel — use standalone agent
    if req.stream {
        match chat_streaming(state, req).await {
            Ok(sse) => sse.into_response(),
            Err((status, msg)) => (status, msg).into_response(),
        }
    } else {
        match chat_sync(state, req).await {
            Ok(json) => json.into_response(),
            Err((status, msg)) => (status, msg).into_response(),
        }
    }
}

fn validate_chat_request(
    state: &AppState,
    req: &ChatRequest,
) -> Result<
    (
        Arc<Agent>,
        Arc<tokio::sync::Mutex<octos_bus::SessionManager>>,
    ),
    (StatusCode, String),
> {
    let agent = state.agent.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "No LLM provider configured. Set up a profile with an API key first.".into(),
    ))?;
    let sessions = state.sessions.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Sessions not available".into(),
    ))?;

    if req.message.len() > MAX_MESSAGE_LEN {
        tracing::warn!(len = req.message.len(), "chat: message exceeds size limit");
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("message exceeds {}KB limit", MAX_MESSAGE_LEN / 1024),
        ));
    }

    Ok((agent.clone(), sessions.clone()))
}

async fn chat_sync(
    state: Arc<AppState>,
    req: ChatRequest,
) -> Result<Json<ChatResponse>, (StatusCode, String)> {
    let (agent, sessions) = validate_chat_request(&state, &req)?;

    tracing::info!(
        session = req.session_id.as_deref().unwrap_or("default"),
        msg_len = req.message.len(),
        "chat: processing message"
    );

    let session_key = SessionKey::new("api", req.session_id.as_deref().unwrap_or("default"));

    let history: Vec<Message> = {
        let mut sess = sessions.lock().await;
        let session = sess.get_or_create(&session_key).await;
        session.get_history(50).to_vec()
    };

    let response = agent
        .process_message(&req.message, &history, vec![])
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "chat: LLM processing failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    tracing::info!(
        input_tokens = response.token_usage.input_tokens,
        output_tokens = response.token_usage.output_tokens,
        "chat: response generated"
    );

    // Save all conversation messages to session
    {
        let mut sess = sessions.lock().await;
        for msg in &response.messages {
            let _ = sess.add_message(&session_key, msg.clone()).await;
        }
    }

    Ok(Json(ChatResponse {
        content: response.content,
        input_tokens: response.token_usage.input_tokens,
        output_tokens: response.token_usage.output_tokens,
    }))
}

async fn chat_streaming(
    state: Arc<AppState>,
    req: ChatRequest,
) -> Result<
    Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, String),
> {
    let (base_agent, sessions) = validate_chat_request(&state, &req)?;

    let session_id = req.session_id.clone().unwrap_or_else(|| "default".into());
    tracing::info!(
        session = %session_id,
        msg_len = req.message.len(),
        "chat: streaming message"
    );

    let session_key = SessionKey::new("api", &session_id);

    // Load history before spawning
    let history: Vec<Message> = {
        let mut sess = sessions.lock().await;
        let session = sess.get_or_create(&session_key).await;
        session.get_history(50).to_vec()
    };

    // Create per-request channel and reporter
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let reporter: Arc<dyn octos_agent::ProgressReporter> = Arc::new(MetricsReporter::new(
        Arc::new(ChannelReporter::new(tx.clone())),
    ));

    // Build per-request agent sharing resources with the base agent
    let request_agent = Agent::new_shared(
        AgentId::new(format!("api-{}", uuid::Uuid::now_v7())),
        base_agent.llm_provider(),
        base_agent.tool_registry().clone(),
        base_agent.memory_store(),
    )
    .with_config(base_agent.agent_config())
    .with_system_prompt(base_agent.system_prompt_snapshot())
    .with_reporter(reporter);

    let message = req.message;
    let media = req.media;

    // Spawn the agent task
    tokio::spawn(async move {
        let result = request_agent
            .process_message(&message, &history, media)
            .await;

        match result {
            Ok(response) => {
                tracing::info!(
                    session = %session_id,
                    input_tokens = response.token_usage.input_tokens,
                    output_tokens = response.token_usage.output_tokens,
                    "chat: streaming response complete"
                );

                // Save all conversation messages (user, assistant iterations, tool calls/results)
                {
                    let mut sess = sessions.lock().await;
                    for msg in &response.messages {
                        let _ = sess.add_message(&session_key, msg.clone()).await;
                    }
                }

                // Send final done event (field names match what octos-web expects)
                let done = serde_json::json!({
                    "type": "done",
                    "content": response.content,
                    "tokens_in": response.token_usage.input_tokens,
                    "tokens_out": response.token_usage.output_tokens,
                });
                let _ = tx.send(done.to_string());
            }
            Err(e) => {
                tracing::error!(session = %session_id, error = %e, "chat: streaming failed");
                let err = serde_json::json!({
                    "type": "error",
                    "message": e.to_string(),
                });
                let _ = tx.send(err.to_string());
            }
        }
        // tx drops here, closing the stream
    });

    // Return SSE stream from receiver
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(data) => {
                let event: Result<Event, std::convert::Infallible> =
                    Ok(Event::default().data(data));
                Some((event, rx))
            }
            None => None,
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// GET /api/chat/stream -- SSE stream of progress events (legacy broadcast).
pub async fn chat_stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = state.broadcaster.subscribe();

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(data) => {
                    let event: Result<Event, std::convert::Infallible> =
                        Ok(Event::default().data(data));
                    return Some((event, rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// GET /api/sessions -- list sessions.
#[derive(Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub message_count: usize,
}

pub async fn list_sessions(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Collect sessions from both the standalone store and gateway profiles,
    // since streaming uses the standalone agent but old sessions may live
    // in gateway stores.
    let mut all: Vec<SessionInfo> = Vec::new();

    if let Some(sessions) = &state.sessions {
        let sess = sessions.lock().await;
        all.extend(sess.list_sessions().into_iter().filter_map(|(id, count)| {
            let chat_id = id.strip_prefix("api:")?;
            Some(SessionInfo {
                id: chat_id.to_string(),
                message_count: count,
            })
        }));
    }

    // Also fetch from gateway if available (old sessions live there)
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let proxy_resp = super::webhook_proxy::api_get_proxy(&state, port, "/sessions").await;
        if proxy_resp.status().is_success() {
            if let Ok(body) = axum::body::to_bytes(proxy_resp.into_body(), 10 * 1024 * 1024).await {
                if let Ok(gateway_sessions) = serde_json::from_slice::<Vec<SessionInfo>>(&body) {
                    // Merge, dedup by id (standalone wins)
                    let existing: std::collections::HashSet<String> =
                        all.iter().map(|s| s.id.clone()).collect();
                    all.extend(
                        gateway_sessions
                            .into_iter()
                            .filter(|s| !existing.contains(&s.id)),
                    );
                }
            }
        }
    }

    if all.is_empty() && state.sessions.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Sessions not available".to_string(),
        )
            .into_response();
    }

    Json(all).into_response()
}

/// GET /api/sessions/:id/messages -- get session history.
///
/// Query params: `?limit=100&offset=0`
#[derive(Deserialize)]
pub struct PaginationParams {
    #[serde(default = "default_page_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub source: Option<String>,
}

fn default_page_limit() -> usize {
    100
}

pub async fn session_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<PaginationParams>,
) -> Response {
    let limit = params.limit.min(500);
    let offset = params.offset.min(10_000);

    // source=full: always proxy to gateway (has per-user + flat JSONL merge)
    let use_full = params.source.as_deref() == Some("full");

    // Try standalone store first (skip for source=full — gateway has the merge logic)
    if !use_full {
    if let Some(sessions) = &state.sessions {
        let fetch_count = match offset.checked_add(limit) {
            Some(n) => n,
            None => return (StatusCode::BAD_REQUEST, "invalid pagination").into_response(),
        };
        let key = SessionKey::new("api", &id);
        let mut sess = sessions.lock().await;
        let session = sess.get_or_create(&key).await;
        let messages: Vec<MessageInfo> = session
            .get_history(fetch_count)
            .iter()
            .skip(offset)
            .take(limit)
            .map(|m| MessageInfo {
                role: m.role.to_string(),
                content: m.content.clone(),
                timestamp: m.timestamp.to_rfc3339(),
            })
            .collect();
        if !messages.is_empty() {
            return Json(messages).into_response();
        }
        // Fall through to gateway for old sessions
    }
    } // !use_full

    // Proxy to gateway (old sessions live there)
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let path = format!(
            "/sessions/{id}/messages?limit={}&offset={}&source=full",
            limit, offset
        );
        return super::webhook_proxy::api_get_proxy(&state, port, &path).await;
    }

    (StatusCode::SERVICE_UNAVAILABLE, "Sessions not available").into_response()
}

#[derive(Serialize)]
pub struct MessageInfo {
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

/// GET /api/sessions/:id/status -- check if session has an active task.
pub async fn session_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    // Proxy to gateway (session actors live there)
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let path = format!("/sessions/{id}/status");
        return super::webhook_proxy::api_get_proxy(&state, port, &path).await;
    }

    // Standalone mode — no active task tracking
    Json(serde_json::json!({
        "active": false,
    }))
    .into_response()
}

/// GET /api/sessions/:id/tasks -- list background tasks for a session.
pub async fn session_tasks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    // Proxy to gateway (task supervisor lives there)
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let path = format!("/sessions/{id}/tasks");
        return super::webhook_proxy::api_get_proxy(&state, port, &path).await;
    }

    // Standalone mode — no background tasks
    Json(serde_json::json!([])).into_response()
}

/// DELETE /api/sessions/:id -- delete a session.
pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if let Some(sessions) = &state.sessions {
        let key = SessionKey::new("api", &id);
        let mut sess = sessions.lock().await;
        return match sess.clear(&key).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => {
                tracing::error!(error = %e, "delete session failed");
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
        };
    }

    // Proxy to gateway
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let path = format!("/sessions/{id}");
        return super::webhook_proxy::api_delete_proxy(&state, port, &path).await;
    }

    (StatusCode::SERVICE_UNAVAILABLE, "Sessions not available").into_response()
}

/// POST /api/upload -- upload files, returns paths for use in /api/chat media field.
///
/// Accepts multipart/form-data with one or more `file` fields.
/// Returns JSON array of server-side file paths.
pub async fn upload(
    State(state): State<Arc<AppState>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    // Determine upload directory
    let upload_dir = std::env::temp_dir().join("octos-uploads");
    tokio::fs::create_dir_all(&upload_dir).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create upload dir: {e}"),
        )
    })?;

    let mut paths = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    let mut total_size: u64 = 0;
    const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50MB per file
    const MAX_TOTAL_SIZE: u64 = 100 * 1024 * 1024; // 100MB total

    while let Ok(Some(field)) = multipart.next_field().await {
        // Only process fields that have a filename (skip non-file fields)
        let filename = match field.file_name() {
            Some(f) => f.to_string(),
            None => continue,
        };
        // Skip duplicate filenames (browser may send the same file twice)
        if !seen_names.insert(filename.clone()) {
            let _ = field.bytes().await; // drain to avoid blocking
            continue;
        }

        // Sanitize filename — strip path separators
        let safe_name = filename
            .replace(['/', '\\', '\0'], "_")
            .chars()
            .take(200)
            .collect::<String>();

        let data = field.bytes().await.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("failed to read field: {e}"),
            )
        })?;

        if data.len() as u64 > MAX_FILE_SIZE {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("file exceeds {MAX_FILE_SIZE} byte limit"),
            ));
        }
        total_size += data.len() as u64;
        if total_size > MAX_TOTAL_SIZE {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "total upload exceeds 100MB".into(),
            ));
        }

        // Unique prefix to avoid collisions
        let dest = upload_dir.join(format!("{}_{safe_name}", uuid::Uuid::now_v7()));
        tokio::fs::write(&dest, &data).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to write file: {e}"),
            )
        })?;

        tracing::info!(path = %dest.display(), size = data.len(), "file uploaded");
        paths.push(dest.to_string_lossy().to_string());
    }

    if paths.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no files in request".into()));
    }

    Ok(Json(paths))
}

/// GET /api/files?path=... -- serve files by query parameter (for absolute paths).
pub async fn serve_file_by_query(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(filename) = params.get("path") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    serve_file_impl(filename).await
}

/// GET /api/files/:filename -- serve uploaded files and pipeline report files.
pub async fn serve_file(axum::extract::Path(filename): axum::extract::Path<String>) -> Response {
    serve_file_impl(&filename).await
}

async fn serve_file_impl(filename: &str) -> Response {
    // Try as an absolute path first (for pipeline-generated files)
    let file_path = std::path::Path::new(&filename);
    let path = if file_path.is_absolute() {
        // Security: only serve files under $HOME/.octos or /tmp
        // (NOT the entire $HOME — prevent reading arbitrary user files)
        let canonical = match std::fs::canonicalize(file_path) {
            Ok(p) => p,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
        let home = std::env::var("HOME").unwrap_or_default();
        let octos_dir = std::fs::canonicalize(format!("{home}/.octos"))
            .unwrap_or_else(|_| std::path::PathBuf::from(format!("{home}/.octos")));
        let tmp_dir =
            std::fs::canonicalize("/tmp").unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
        let allowed = canonical.starts_with(&octos_dir) || canonical.starts_with(&tmp_dir);
        if !allowed {
            return (StatusCode::FORBIDDEN, "access denied").into_response();
        }
        canonical
    } else {
        // Relative path — serve from uploads dir
        let safe_name = filename.replace(['/', '\\', '\0', '~'], "_");
        let upload_dir = std::env::temp_dir().join("octos-uploads");
        let path = upload_dir.join(&safe_name);
        if !path.exists() || !path.starts_with(&upload_dir) {
            return StatusCode::NOT_FOUND.into_response();
        }
        path
    };

    let data = match tokio::fs::read(&path).await {
        Ok(d) => d,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    // Detect content type from extension
    let content_type = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("ogg") => "audio/ogg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("pdf") => "application/pdf",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    };

    let display_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| filename.to_string())
        .replace(['"', '\r', '\n', '\\'], "_");

    let mut headers = axum::http::HeaderMap::new();
    headers.insert("content-type", content_type.parse().unwrap());
    headers.insert(
        "content-disposition",
        format!("inline; filename=\"{display_name}\"")
            .parse()
            .unwrap(),
    );

    (StatusCode::OK, headers, data).into_response()
}

/// GET /api/files/list?dirs=research,slides,skill-output — list files in profile content directories.
pub async fn list_content_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    // Resolve profile data dir
    let data_dir = if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        // Proxy approach: ask the gateway. But simpler: derive from profile store.
        if let Some(ref ps) = state.profile_store {
            if let Some(pid) = headers.get("x-profile-id").and_then(|v| v.to_str().ok()) {
                if let Ok(Some(p)) = ps.get(pid) {
                    ps.resolve_data_dir(&p)
                } else {
                    return (StatusCode::NOT_FOUND, "profile not found").into_response();
                }
            } else {
                return (StatusCode::BAD_REQUEST, "missing X-Profile-Id").into_response();
            }
        } else {
            return (StatusCode::SERVICE_UNAVAILABLE, "no profile store").into_response();
        }
    } else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no gateway").into_response();
    };

    let dirs_param = params.get("dirs").cloned().unwrap_or_else(|| "research,slides,skill-output".to_string());
    let mut scan_dirs: Vec<String> = dirs_param.split(',').map(|s| s.trim().to_string()).collect();

    // Scan per-user workspace directories that match the requested dirs.
    // Only use original relative dir names — absolute paths from prior
    // iterations would bypass ws.join() (Path::join replaces on absolute).
    let requested_dirs: Vec<String> = scan_dirs.clone();
    let users_dir = data_dir.join("users");
    if let Ok(entries) = std::fs::read_dir(&users_dir) {
        for entry in entries.flatten() {
            let ws = entry.path().join("workspace");
            if !ws.exists() { continue; }
            for dir_name in &requested_dirs {
                if std::path::Path::new(dir_name.as_str()).is_absolute() { continue; }
                let ws_dir = ws.join(dir_name);
                if ws_dir.exists() && ws_dir.is_dir() {
                    scan_dirs.push(ws_dir.to_string_lossy().to_string());
                }
            }
        }
    }

    #[derive(Serialize)]
    struct ContentFile {
        filename: String,
        path: String,
        size: u64,
        modified: String,
        category: String,
        /// Parent directory name for grouping in the UI
        group: String,
    }

    // Only show final output files, skip all intermediate/internal files
    fn is_output_file(filename: &str) -> bool {
        let lower = filename.to_lowercase();
        // Skip hidden files
        if lower.starts_with('.') { return false; }
        // Skip research intermediate files
        if lower.starts_with('_') { return false; } // _report.md, _search_results.md, _sources.json
        // Skip intermediates
        if lower.starts_with("panel-") { return false; }
        if lower.contains("-ref.") { return false; } // mofa reference images
        // Only keep meaningful output extensions
        matches!(
            lower.rsplit('.').next().unwrap_or(""),
            "md" | "txt" | "pptx" | "pdf" | "docx" | "xlsx" | "png" | "jpg" | "mp3" | "wav" | "mp4" | "js" | "json"
        )
    }

    fn modified_rfc3339(meta: &std::fs::Metadata) -> String {
        meta.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                .map(|dt| dt.to_rfc3339()).unwrap_or_default())
            .unwrap_or_default()
    }

    let mut files = Vec::new();
    for dir_name in &scan_dirs {
        let dir_path = if std::path::Path::new(dir_name.as_str()).is_absolute() {
            std::path::PathBuf::from(dir_name.as_str())
        } else {
            data_dir.join(dir_name.as_str())
        };
        if !dir_path.exists() { continue; }
        if let Ok(entries) = std::fs::read_dir(&dir_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                let display_dir = if std::path::Path::new(dir_name.as_str()).is_absolute() {
                    // For workspace paths, show "workspace/research"
                    let p = std::path::Path::new(dir_name.as_str());
                    let parts: Vec<&str> = p.components().rev().take(2).map(|c| c.as_os_str().to_str().unwrap_or("")).collect();
                    parts.into_iter().rev().collect::<Vec<_>>().join("/")
                } else {
                    dir_name.to_string()
                };
                let group_name = format!("{}/{}", display_dir, path.file_name().unwrap_or_default().to_string_lossy());
                if path.is_dir() {
                    // For skill-output and slides: scan subdirs for final outputs
                    // For research: skip subdirs (they contain intermediate search results)
                    if *dir_name != "research" {
                        // Collect dirs to scan (up to 2 levels: output/ and output/imgs/)
                        let mut subdirs = vec![path.clone()];
                        if let Ok(sub_entries) = std::fs::read_dir(&path) {
                            for sub in sub_entries.flatten() {
                                if sub.path().is_dir() {
                                    subdirs.push(sub.path());
                                }
                            }
                        }
                        for subdir in &subdirs {
                            if let Ok(sub_entries) = std::fs::read_dir(subdir) {
                                for sub in sub_entries.flatten() {
                                    let sp = sub.path();
                                    if sp.is_file() {
                                        let filename = sp.file_name().unwrap_or_default().to_string_lossy().to_string();
                                        if !is_output_file(&filename) { continue; }
                                        if let Ok(meta) = sp.metadata() {
                                            files.push(ContentFile {
                                                category: categorize(&filename),
                                                filename,
                                                path: sp.to_string_lossy().to_string(),
                                                size: meta.len(),
                                                modified: modified_rfc3339(&meta),
                                                group: group_name.clone(),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else if path.is_file() {
                    let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    if !is_output_file(&filename) { continue; }
                    if let Ok(meta) = path.metadata() {
                        files.push(ContentFile {
                            category: categorize(&filename),
                            filename,
                            path: path.to_string_lossy().to_string(),
                            size: meta.len(),
                            modified: modified_rfc3339(&meta),
                            group: display_dir.clone(),
                        });
                    }
                }
            }
        }
    }

    // Sort by modified desc, limit to 100 most recent
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    files.truncate(100);
    Json(files).into_response()
}

fn categorize(filename: &str) -> String {
    let lower = filename.to_lowercase();
    if lower.ends_with(".md") || lower.ends_with(".txt") { "report".into() }
    else if lower.ends_with(".pptx") || lower.ends_with(".pdf") { "slides".into() }
    else if lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.ends_with(".webp") { "image".into() }
    else if lower.ends_with(".mp3") || lower.ends_with(".wav") || lower.ends_with(".ogg") { "audio".into() }
    else if lower.ends_with(".mp4") || lower.ends_with(".webm") { "video".into() }
    else { "other".into() }
}

/// GET /api/status -- server status.
#[derive(Serialize)]
pub struct StatusResponse {
    pub version: String,
    pub model: String,
    pub provider: String,
    pub uptime_secs: i64,
    pub agent_configured: bool,
}

pub async fn status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let uptime = chrono::Utc::now() - state.started_at;
    let (model, provider) = match &state.agent {
        Some(agent) => (
            agent.model_id().to_string(),
            agent.provider_name().to_string(),
        ),
        None => ("none".to_string(), "none".to_string()),
    };
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        model,
        provider,
        uptime_secs: uptime.num_seconds(),
        agent_configured: state.agent.is_some(),
    })
}

/// GET /api/version — public version endpoint (no auth required).
pub async fn version() -> Json<serde_json::Value> {
    let version = env!("CARGO_PKG_VERSION");
    let git_hash = option_env!("OCTOS_GIT_HASH").unwrap_or("");
    let build_date = option_env!("OCTOS_BUILD_DATE").unwrap_or("");
    let full = if git_hash.is_empty() {
        version.to_string()
    } else {
        format!("{version}+{git_hash}")
    };
    Json(serde_json::json!({
        "service": "octos",
        "version": full,
        "build_date": build_date,
    }))
}

/// GET /health — public health check (no auth required).
pub async fn health() -> Json<serde_json::Value> {
    let version = env!("CARGO_PKG_VERSION");
    let git_hash = option_env!("OCTOS_GIT_HASH").unwrap_or("");
    let full = if git_hash.is_empty() {
        version.to_string()
    } else {
        format!("{version}+{git_hash}")
    };
    Json(serde_json::json!({
        "status": "healthy",
        "service": "octos",
        "version": full,
    }))
}

// ---------------------------------------------------------------------------
// WebSocket endpoint
// ---------------------------------------------------------------------------

/// Client → Server message protocol over WebSocket.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsClientMsg {
    /// Send a chat message (equivalent to POST /api/chat).
    Send {
        content: String,
        #[serde(default)]
        media: Vec<String>,
        #[serde(default)]
        session: Option<String>,
    },
    /// Abort the current streaming response.
    Abort,
}

/// GET /api/ws?session={session_id}&token={token} — WebSocket endpoint.
///
/// Provides bidirectional real-time communication as an alternative to the
/// SSE-based streaming flow. Server→Client events use the same JSON format
/// as SSE events. Client→Server commands: `send` and `abort`.
pub async fn ws_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Extract session_id from query params.
    // (Auth is already handled by the user_auth_middleware layer.)
    ws.on_upgrade(move |socket| ws_connection(socket, state, headers))
}

/// Handle an established WebSocket connection.
async fn ws_connection(socket: WebSocket, state: Arc<AppState>, headers: HeaderMap) {
    let (ws_tx, mut ws_rx) = socket.split();
    let ws_tx = Arc::new(tokio::sync::Mutex::new(ws_tx));

    // Track the abort handle for the current streaming task so clients can
    // cancel in-flight requests.
    let abort_handle: Arc<tokio::sync::Mutex<Option<tokio::task::AbortHandle>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    while let Some(Ok(msg)) = ws_rx.next().await {
        let text = match msg {
            WsMessage::Text(t) => t,
            WsMessage::Close(_) => break,
            // Respond to pings with pongs (axum handles this automatically in
            // most cases, but be explicit).
            WsMessage::Ping(_) => continue,
            _ => continue,
        };

        let client_msg: WsClientMsg = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                let err = serde_json::json!({"type": "error", "message": format!("invalid message: {e}")});
                let _ = send_ws(&ws_tx, &err.to_string()).await;
                continue;
            }
        };

        match client_msg {
            WsClientMsg::Send {
                content,
                media,
                session,
            } => {
                if content.len() > MAX_MESSAGE_LEN {
                    let err = serde_json::json!({
                        "type": "error",
                        "message": format!("message exceeds {}KB limit", MAX_MESSAGE_LEN / 1024),
                    });
                    let _ = send_ws(&ws_tx, &err.to_string()).await;
                    continue;
                }

                let session_id = session.unwrap_or_else(|| "default".into());

                // If a gateway is running, proxy through it (same as chat handler).
                if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
                    let ws_tx2 = ws_tx.clone();
                    let _abort_ref = abort_handle.clone();
                    let http_client = state.http_client.clone();
                    let handle = tokio::spawn(async move {
                        ws_proxy_to_gateway(
                            ws_tx2,
                            &http_client,
                            port,
                            &content,
                            Some(&session_id),
                            &media,
                        )
                        .await;
                    });
                    *abort_handle.lock().await = Some(handle.abort_handle());
                } else if let Ok((agent, sessions)) = validate_chat_request(
                    &state,
                    &ChatRequest {
                        message: content.clone(),
                        session_id: Some(session_id.clone()),
                        stream: true,
                        media: media.clone(),
                    },
                ) {
                    // Standalone agent mode — run the agent directly.
                    let ws_tx2 = ws_tx.clone();
                    let _abort_ref = abort_handle.clone();
                    let handle = tokio::spawn(async move {
                        ws_standalone_agent(ws_tx2, agent, sessions, &session_id, &content, media)
                            .await;
                    });
                    *abort_handle.lock().await = Some(handle.abort_handle());
                } else {
                    let err = serde_json::json!({
                        "type": "error",
                        "message": "No LLM provider configured",
                    });
                    let _ = send_ws(&ws_tx, &err.to_string()).await;
                }
            }
            WsClientMsg::Abort => {
                if let Some(handle) = abort_handle.lock().await.take() {
                    handle.abort();
                    let msg = serde_json::json!({"type": "error", "message": "aborted"});
                    let _ = send_ws(&ws_tx, &msg.to_string()).await;
                }
            }
        }
    }
}

/// Proxy a WebSocket chat request to the gateway's internal API channel and
/// stream SSE events back as WebSocket text frames.
async fn ws_proxy_to_gateway(
    ws_tx: Arc<tokio::sync::Mutex<futures::stream::SplitSink<WebSocket, WsMessage>>>,
    http_client: &reqwest::Client,
    port: u16,
    message: &str,
    session_id: Option<&str>,
    media: &[String],
) {
    use futures::StreamExt;

    let url = format!("http://127.0.0.1:{port}/chat");
    let body = serde_json::json!({
        "message": message,
        "session_id": session_id,
        "media": media,
    });

    let resp = match http_client
        .post(&url)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let err = serde_json::json!({"type": "error", "message": format!("gateway proxy failed: {e}")});
            let _ = send_ws(&ws_tx, &err.to_string()).await;
            return;
        }
    };

    if !resp.status().is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        let err = serde_json::json!({"type": "error", "message": err_body});
        let _ = send_ws(&ws_tx, &err.to_string()).await;
        return;
    }

    // Stream SSE events from the gateway response and forward as WS text frames.
    // The gateway sends `text/event-stream` with `data: {...}\n\n` lines.
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(_) => break,
        };
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(_) => continue,
        };

        buffer.push_str(text);

        // Parse SSE frames: lines starting with "data:" separated by blank lines.
        while let Some(pos) = buffer.find("\n\n") {
            let frame = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();

            for line in frame.lines() {
                let data = if let Some(d) = line.strip_prefix("data:") {
                    d.trim()
                } else if let Some(d) = line.strip_prefix("data: ") {
                    d.trim()
                } else {
                    continue;
                };
                if data.is_empty() {
                    continue;
                }
                if send_ws(&ws_tx, data).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Run the standalone agent for a WebSocket request and stream events back.
async fn ws_standalone_agent(
    ws_tx: Arc<tokio::sync::Mutex<futures::stream::SplitSink<WebSocket, WsMessage>>>,
    base_agent: Arc<Agent>,
    sessions: Arc<tokio::sync::Mutex<octos_bus::SessionManager>>,
    session_id: &str,
    message: &str,
    media: Vec<String>,
) {
    let session_key = SessionKey::new("api", session_id);

    let history: Vec<Message> = {
        let mut sess = sessions.lock().await;
        let session = sess.get_or_create(&session_key).await;
        session.get_history(50).to_vec()
    };

    // Create per-request channel and reporter
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let reporter: Arc<dyn octos_agent::ProgressReporter> = Arc::new(MetricsReporter::new(
        Arc::new(ChannelReporter::new(tx.clone())),
    ));

    let request_agent = Agent::new_shared(
        AgentId::new(format!("ws-{}", uuid::Uuid::now_v7())),
        base_agent.llm_provider(),
        base_agent.tool_registry().clone(),
        base_agent.memory_store(),
    )
    .with_config(base_agent.agent_config())
    .with_system_prompt(base_agent.system_prompt_snapshot())
    .with_reporter(reporter);

    let message = message.to_string();
    let session_id = session_id.to_string();
    let session_key2 = SessionKey::new("api", &session_id);

    // Spawn the agent task
    tokio::spawn(async move {
        let result = request_agent
            .process_message(&message, &history, media)
            .await;

        match result {
            Ok(response) => {
                // Save conversation messages to session
                {
                    let mut sess = sessions.lock().await;
                    for msg in &response.messages {
                        let _ = sess.add_message(&session_key2, msg.clone()).await;
                    }
                }

                let done = serde_json::json!({
                    "type": "done",
                    "content": response.content,
                    "tokens_in": response.token_usage.input_tokens,
                    "tokens_out": response.token_usage.output_tokens,
                });
                let _ = tx.send(done.to_string());
            }
            Err(e) => {
                let err = serde_json::json!({
                    "type": "error",
                    "message": e.to_string(),
                });
                let _ = tx.send(err.to_string());
            }
        }
    });

    // Forward channel events to WebSocket
    while let Some(data) = rx.recv().await {
        if send_ws(&ws_tx, &data).await.is_err() {
            break;
        }
    }
}

/// Send a text message through the WebSocket sink.
async fn send_ws(
    ws_tx: &Arc<tokio::sync::Mutex<futures::stream::SplitSink<WebSocket, WsMessage>>>,
    data: &str,
) -> Result<(), ()> {
    use futures::SinkExt;
    let mut tx = ws_tx.lock().await;
    tx.send(WsMessage::text(data)).await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_deserialize() {
        let json = r#"{"message": "hello"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "hello");
        assert!(req.session_id.is_none());
        assert!(!req.stream);
    }

    #[test]
    fn chat_request_with_session() {
        let json = r#"{"message": "hi", "session_id": "s1"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "hi");
        assert_eq!(req.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn chat_request_with_stream() {
        let json = r#"{"message": "hi", "stream": true}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert!(req.stream);
    }

    #[test]
    fn chat_response_serialize() {
        let resp = ChatResponse {
            content: "world".into(),
            input_tokens: 10,
            output_tokens: 5,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["content"], "world");
        assert_eq!(json["input_tokens"], 10);
        assert_eq!(json["output_tokens"], 5);
    }

    #[test]
    fn session_info_serialize() {
        let info = SessionInfo {
            id: "test-session".into(),
            message_count: 42,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["id"], "test-session");
        assert_eq!(json["message_count"], 42);
    }

    #[test]
    fn message_info_serialize() {
        let info = MessageInfo {
            role: "user".into(),
            content: "hello".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
        assert_eq!(json["timestamp"], "2025-01-01T00:00:00Z");
    }

    #[test]
    fn status_response_serialize() {
        let resp = StatusResponse {
            version: "0.1.0".into(),
            model: "gpt-4".into(),
            provider: "openai".into(),
            uptime_secs: 120,
            agent_configured: true,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["version"], "0.1.0");
        assert_eq!(json["model"], "gpt-4");
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["uptime_secs"], 120);
        assert_eq!(json["agent_configured"], true);
    }

    #[test]
    fn pagination_defaults() {
        let json = r#"{}"#;
        let params: PaginationParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 100);
        assert_eq!(params.offset, 0);
    }

    #[test]
    fn pagination_custom_values() {
        let json = r#"{"limit": 50, "offset": 10}"#;
        let params: PaginationParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 50);
        assert_eq!(params.offset, 10);
    }

    #[test]
    fn default_page_limit_is_100() {
        assert_eq!(default_page_limit(), 100);
    }

    #[test]
    fn max_message_len_is_1mb() {
        assert_eq!(MAX_MESSAGE_LEN, 1_048_576);
    }

    #[test]
    fn ws_client_msg_send_deserialize() {
        let json = r#"{"type": "send", "content": "hello"}"#;
        let msg: WsClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMsg::Send {
                content,
                media,
                session,
            } => {
                assert_eq!(content, "hello");
                assert!(media.is_empty());
                assert!(session.is_none());
            }
            _ => panic!("expected Send"),
        }
    }

    #[test]
    fn ws_client_msg_send_with_session_and_media() {
        let json = r#"{"type": "send", "content": "hi", "session": "s1", "media": ["/tmp/a.png"]}"#;
        let msg: WsClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMsg::Send {
                content,
                media,
                session,
            } => {
                assert_eq!(content, "hi");
                assert_eq!(session.as_deref(), Some("s1"));
                assert_eq!(media, vec!["/tmp/a.png"]);
            }
            _ => panic!("expected Send"),
        }
    }

    #[test]
    fn ws_client_msg_abort_deserialize() {
        let json = r#"{"type": "abort"}"#;
        let msg: WsClientMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, WsClientMsg::Abort));
    }

    #[test]
    fn ws_client_msg_invalid_type() {
        let json = r#"{"type": "unknown"}"#;
        let result = serde_json::from_str::<WsClientMsg>(json);
        assert!(result.is_err());
    }
}
