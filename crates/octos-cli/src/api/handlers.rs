//! API request handlers.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};

use axum::Extension;
use axum::Json;
use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::StreamExt;
use octos_agent::{Agent, inspect_workspace_contract};
use octos_bus::file_handle::{
    encode_profile_file_handle, encode_tmp_upload_handle, resolve_legacy_file_request,
    resolve_scoped_file_handle,
};
use octos_core::{AgentId, MAIN_PROFILE_ID, Message, MessageRole, SessionKey};
use octos_llm::pricing::model_pricing;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth_handlers::ADMIN_PROFILE_ID;
use super::metrics::MetricsReporter;
use super::router::AuthIdentity;
use super::sse::ChannelReporter;
use crate::project_templates::{SiteProjectMetadata, read_site_project_metadata};

/// POST /api/chat -- send a message, get a response.
/// When `stream: true`, returns SSE events. Otherwise returns JSON.
#[derive(Deserialize)]
pub struct ChatRequest {
    pub message: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub stream: bool,
    /// File paths from prior `/api/upload` call.
    #[serde(default)]
    pub media: Vec<String>,
    #[serde(default)]
    pub attach_only: bool,
    /// Web-generated correlation id. Forwarded to the gateway so the
    /// eventual `_session_result.response_to_client_message_id` matches
    /// the web reducer's optimistic bubble (FA-12f).
    ///
    /// Also propagated onto the persisted user `Message` so the matching
    /// `session_result` event lets the web client stamp the authoritative
    /// `historySeq` onto its optimistic bubble.
    #[serde(default)]
    pub client_message_id: Option<String>,
}

#[derive(Serialize)]
pub struct ChatResponse {
    pub content: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Serialize)]
pub(crate) struct ContentFileEntry {
    filename: String,
    path: String,
    size: u64,
    modified: String,
    category: String,
    /// Parent directory name for grouping in the UI.
    group: String,
}

pub(crate) fn response_path_for_profile_file(
    base_dir: &std::path::Path,
    path: &std::path::Path,
) -> Option<String> {
    encode_profile_file_handle(base_dir, path)
        .or_else(|| encode_tmp_upload_handle(path, path.file_name().and_then(|name| name.to_str())))
}

fn resolve_scoped_download_path(
    base_dir: &std::path::Path,
    request_path: &str,
) -> Option<std::path::PathBuf> {
    resolve_scoped_file_handle(base_dir, request_path)
        .or_else(|| resolve_legacy_file_request(base_dir, request_path))
}

/// Maximum message length (1MB).
const MAX_MESSAGE_LEN: usize = 1_048_576;

fn request_host(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .to_ascii_lowercase();
    if raw.is_empty() {
        return None;
    }
    Some(strip_port_from_host(&raw).to_string())
}

fn strip_port_from_host(host: &str) -> &str {
    if let Some(stripped) = host.strip_prefix('[') {
        return stripped.split(']').next().unwrap_or(host);
    }

    if host.matches(':').count() == 1 {
        return host.split(':').next().unwrap_or(host);
    }

    host
}

fn is_local_request_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn resolve_profile_id_candidate(state: &AppState, candidate: &str) -> Option<String> {
    state
        .profile_store
        .as_ref()
        .and_then(|store| store.resolve_routable_profile_id(candidate).ok().flatten())
}

fn routed_profile_id_from_headers(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if let Some(host) = request_host(headers) {
        if !is_local_request_host(&host) {
            if let Some(candidate) = host.split('.').next() {
                if let Some(profile_id) = resolve_profile_id_candidate(state, candidate) {
                    return Some(profile_id);
                }
            }
        }
    }

    headers
        .get("x-profile-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|candidate| resolve_profile_id_candidate(state, candidate))
}

/// Resolve API port for a specific profile, or fall back to first available.
/// Profile is identified by X-Profile-Id header (set by Caddy from subdomain).
async fn resolve_api_port(state: &AppState, headers: &HeaderMap) -> Option<(String, u16)> {
    let pm = state.process_manager.as_ref()?;

    if let Some(profile_id) = routed_profile_id_from_headers(state, headers) {
        if let Some(port) = pm.api_port(&profile_id).await {
            return Some((profile_id, port));
        }
        tracing::warn!(profile = profile_id, "no API port for requested profile");
    }

    // Fall back to first available
    pm.first_api_port().await
}

fn api_profile_id_from_headers(state: &AppState, headers: &HeaderMap) -> String {
    routed_profile_id_from_headers(state, headers).unwrap_or_else(|| MAIN_PROFILE_ID.to_string())
}

fn standalone_api_session_key_candidates(
    state: &AppState,
    headers: &HeaderMap,
    session_id: &str,
) -> Vec<SessionKey> {
    let profile_id = api_profile_id_from_headers(state, headers);
    let mut candidates = vec![
        SessionKey::with_profile(&profile_id, "api", session_id),
        SessionKey::with_profile(MAIN_PROFILE_ID, "api", session_id),
        SessionKey::new("api", session_id),
    ];
    candidates.dedup_by(|left, right| left.0 == right.0);
    candidates
}

fn encode_api_session_path_id(id: &str) -> String {
    octos_bus::session::encode_path_component(id)
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
    if let Some((profile_id, port)) = resolve_api_port(&state, &headers).await {
        return super::webhook_proxy::api_chat_proxy(
            &state,
            port,
            Some(&profile_id),
            &req.message,
            req.session_id.as_deref(),
            req.topic.as_deref(),
            &req.media,
            req.attach_only,
            req.stream,
            req.client_message_id.as_deref(),
        )
        .await;
    }

    // No gateway with API channel — use standalone agent
    if req.stream {
        match chat_streaming(state, headers, req).await {
            Ok(sse) => sse.into_response(),
            Err((status, msg)) => (status, msg).into_response(),
        }
    } else {
        match chat_sync(state, headers, req).await {
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

/// Persist a `Message` to the canonical per-user `<topic>.jsonl` and
/// invalidate the `SessionManager` LRU cache for the key.
///
/// This is the unified write path for the standalone `octos serve` /chat
/// handlers. Mirrors `ApiChannel::persist_to_session` in the gateway path
/// — both funnel through `octos_bus::persist_message_through_canonical_path`
/// so the storage layer is the single ordering point.
///
/// Returns the committed per-session sequence number so callers (e.g. the
/// streaming handler) can correlate it back to the optimistic bubble.
async fn persist_chat_message_through_canonical(
    sessions: &Arc<tokio::sync::Mutex<octos_bus::SessionManager>>,
    key: &SessionKey,
    message: Message,
) -> eyre::Result<usize> {
    let data_dir = {
        let manager = sessions.lock().await;
        manager.data_dir()
    };

    let result = octos_bus::persist_message_through_canonical_path(&data_dir, key, message).await;

    // Drop any stale `SessionManager` cache entry so a follow-up read
    // (duplicate-detection, `?source=full`) consults disk instead of
    // returning a pre-write empty `Session`.
    {
        let mut manager = sessions.lock().await;
        manager.invalidate_cache(key);
    }

    result
}

async fn chat_sync(
    state: Arc<AppState>,
    headers: HeaderMap,
    req: ChatRequest,
) -> Result<Json<ChatResponse>, (StatusCode, String)> {
    let (agent, sessions) = validate_chat_request(&state, &req)?;

    tracing::info!(
        session = req.session_id.as_deref().unwrap_or("default"),
        msg_len = req.message.len(),
        "chat: processing message"
    );

    let session_key = standalone_api_session_key_with_topic(
        &state,
        &headers,
        req.session_id.as_deref().unwrap_or("default"),
        req.topic.as_deref(),
    );

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

    // Save all conversation messages to the canonical per-user JSONL.
    // Funnels through the same helper the gateway-side `ApiChannel` uses so
    // standalone deployments don't split-brain into the legacy flat layout.
    for msg in &response.messages {
        let _ = persist_chat_message_through_canonical(&sessions, &session_key, msg.clone()).await;
    }

    Ok(Json(ChatResponse {
        content: response.content,
        input_tokens: response.token_usage.input_tokens,
        output_tokens: response.token_usage.output_tokens,
    }))
}

async fn chat_streaming(
    state: Arc<AppState>,
    headers: HeaderMap,
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

    let session_key =
        standalone_api_session_key_with_topic(&state, &headers, &session_id, req.topic.as_deref());

    // Load history before spawning
    let history: Vec<Message> = {
        let mut sess = sessions.lock().await;
        let session = sess.get_or_create(&session_key).await;
        session.get_history(50).to_vec()
    };

    // Create per-request channel and reporter.
    //
    // M8.10 PR #2: bind the user message's `client_message_id` to the
    // reporter so every emitted SSE payload carries `thread_id`. The
    // standalone `serve` mode shares a single chat_id across turns, but
    // each turn gets a fresh ChannelReporter scoped to its cmid.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let client_message_id = req.client_message_id.clone();
    let reporter: Arc<dyn octos_agent::ProgressReporter> = Arc::new(MetricsReporter::new(
        Arc::new(ChannelReporter::new(tx.clone()).with_thread_id(client_message_id.clone())),
    ));

    // Build per-request agent sharing resources with the base agent
    let mut request_agent = Agent::new_shared(
        AgentId::new(format!("api-{}", uuid::Uuid::now_v7())),
        base_agent.llm_provider(),
        base_agent.tool_registry().clone(),
        base_agent.memory_store(),
    )
    .with_config(base_agent.agent_config())
    .with_system_prompt(base_agent.system_prompt_snapshot())
    .with_reporter(reporter);

    // M8 fix-first item 8 (gaps 1 + 2): `Agent::new_shared` zeroes the
    // file-state cache and sub-agent router/generator. Per-request agents
    // must inherit them from the base agent so chat requests land on the
    // same M8 wiring the rest of the runtime sees.
    if let Some(cache) = base_agent.file_state_cache() {
        request_agent = request_agent.with_file_state_cache(cache.clone());
    }
    if let Some(router) = base_agent.subagent_output_router() {
        request_agent = request_agent.with_subagent_output_router(router.clone());
    }
    if let Some(generator) = base_agent.subagent_summary_generator() {
        request_agent = request_agent.with_subagent_summary_generator(generator.clone());
    }

    let message = req.message;
    let media = req.media;
    let topic_for_event = req.topic.clone();

    // Spawn the agent task
    let user_event_tx = tx.clone();
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

                // Save all conversation messages (user, assistant iterations,
                // tool calls/results) through the canonical per-user JSONL.
                // Pre-fix this funnelled through `SessionManager::add_message_with_seq`
                // which wrote to the legacy flat layout — a standalone
                // `octos serve` had no gateway-side `ApiChannel` to redirect,
                // so messages landed in `sessions/<encoded_full_key>.jsonl`
                // while the actor wrote to `users/.../<topic>.jsonl`.
                //
                // Also tag the first user message with the client-supplied
                // `client_message_id` so the persisted row carries it through
                // the JSONL round-trip and emit a user-message session_result
                // event so the web client can stamp the authoritative seq onto
                // its optimistic bubble (M8.10-A user-message counterpart).
                //
                // Capture the committed seq of the final assistant message
                // so the SSE `done` event can thread it back to the web client
                // (M8.10-A).
                let mut user_message_seq_and_meta: Option<(usize, String, String)> = None;
                let assistant_committed_seq: Option<u64> = {
                    let mut last_assistant_seq: Option<u64> = None;
                    let mut user_persisted = false;
                    for msg in &response.messages {
                        let mut to_save = msg.clone();
                        if !user_persisted && msg.role == MessageRole::User {
                            user_persisted = true;
                            // PR A: stamp via the typed setter so callers
                            // wired to a `ClientMessageId` can't pass the
                            // wrong identity here. Bare-`String` overrides
                            // remain available for inbound paths where the
                            // cmid is already a `String` from the wire.
                            if let Some(ref cmid) = client_message_id {
                                if !cmid.is_empty() {
                                    to_save = to_save.with_typed_client_message_id(
                                        octos_core::ClientMessageId::new(cmid),
                                    );
                                }
                            }
                            let timestamp = to_save.timestamp.to_rfc3339();
                            let content_for_event = to_save.content.clone();
                            match persist_chat_message_through_canonical(
                                &sessions,
                                &session_key,
                                to_save,
                            )
                            .await
                            {
                                Ok(seq) => {
                                    user_message_seq_and_meta =
                                        Some((seq, content_for_event, timestamp));
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        session = %session_id,
                                        error = %error,
                                        "chat: failed to persist user message"
                                    );
                                }
                            }
                        } else {
                            let is_assistant = msg.role == MessageRole::Assistant;
                            match persist_chat_message_through_canonical(
                                &sessions,
                                &session_key,
                                to_save,
                            )
                            .await
                            {
                                Ok(seq) if is_assistant => {
                                    last_assistant_seq = u64::try_from(seq).ok();
                                }
                                Ok(_) => {}
                                Err(_) => {}
                            }
                        }
                    }
                    last_assistant_seq
                };

                // Emit a user-message session_result event so the web client
                // can stamp the authoritative seq onto its optimistic bubble.
                if let Some((seq, content, timestamp)) = user_message_seq_and_meta {
                    let mut message_payload = serde_json::json!({
                        "seq": seq,
                        "role": "user",
                        "content": content,
                        "timestamp": timestamp,
                    });
                    if let Some(ref cmid) = client_message_id {
                        if !cmid.is_empty() {
                            message_payload
                                .as_object_mut()
                                .expect("json object")
                                .insert(
                                    "client_message_id".to_string(),
                                    serde_json::Value::String(cmid.clone()),
                                );
                        }
                    }
                    let event = serde_json::json!({
                        "type": "session_result",
                        "topic": topic_for_event,
                        "message": message_payload,
                    });
                    let _ = user_event_tx.send(event.to_string());
                }

                // Send final done event (field names match what octos-web expects)
                let provider_metadata = response.provider_metadata.clone();
                let model_id = provider_metadata
                    .as_ref()
                    .map(|meta| meta.model.clone())
                    .or_else(|| {
                        let provider = request_agent.llm_provider();
                        let model = provider.model_id();
                        if model.is_empty() {
                            None
                        } else {
                            Some(model.to_string())
                        }
                    });
                let session_cost = model_id.as_deref().and_then(model_pricing).map(|pricing| {
                    pricing.cost(
                        response.token_usage.input_tokens,
                        response.token_usage.output_tokens,
                    )
                });
                let mut done = serde_json::json!({
                    "type": "done",
                    "content": response.content,
                    "model": provider_metadata.as_ref().map(|meta| meta.display_label()),
                    "provider": provider_metadata.as_ref().map(|meta| meta.provider.clone()),
                    "model_id": model_id,
                    "endpoint": provider_metadata.as_ref().and_then(|meta| meta.endpoint.clone()),
                    "tokens_in": response.token_usage.input_tokens,
                    "tokens_out": response.token_usage.output_tokens,
                    "session_cost": session_cost,
                });
                if let Some(seq) = assistant_committed_seq {
                    done["committed_seq"] = serde_json::Value::from(seq);
                }
                // M8.10 PR #2: tag the done event with thread_id so the web
                // client can route the committed_seq onto the right per-cmid
                // bubble.
                if let Some(ref tid) = client_message_id {
                    if !tid.is_empty() {
                        done["thread_id"] = serde_json::Value::String(tid.clone());
                    }
                }
                // Bug 3 / W1.G4 cost panel — flatten per-node cost rows from
                // tool results' structured side-channel into the SSE done
                // event so the dashboard CostBreakdown panel can render
                // real per-node attribution from `run_pipeline` runs.
                let mut all_node_costs: Vec<serde_json::Value> = Vec::new();
                for (_tool_call_id, meta) in &response.tool_results {
                    if let Some(arr) = meta.get("node_costs").and_then(|v| v.as_array()) {
                        all_node_costs.extend(arr.iter().cloned());
                    }
                }
                if !all_node_costs.is_empty() {
                    done["node_costs"] = serde_json::Value::Array(all_node_costs);
                }
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
    /// Display title (auto-derived from first user message; manual rename via
    /// PATCH /api/sessions/:id/title preserves across new messages). None
    /// for legacy sessions persisted before the title field existed; the
    /// client should fall back to deriving a title from message content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

fn is_internal_api_session_id(id: &str) -> bool {
    id.split_once('#')
        .is_some_and(|(_, topic)| is_internal_session_topic(topic))
}

fn is_internal_session_topic(topic: &str) -> bool {
    topic.starts_with("child-") || topic == "default.tasks" || topic.ends_with(".tasks")
}

pub async fn list_sessions(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Collect sessions from both the standalone store and gateway profiles.
    let mut all: Vec<SessionInfo> = Vec::new();

    if let Some(sessions) = &state.sessions {
        let sess = sessions.lock().await;
        let prefix = format!("{}:api:", api_profile_id_from_headers(&state, &headers));
        // Use `list_top_level_sessions` (skips `child-*` and `*.tasks` at the
        // directory walk) so a user dir with tens of thousands of spawn
        // children does not turn this listing into an O(N) hang. The
        // `is_internal_api_session_id` belt-and-suspenders check is kept for
        // legacy entries that might slip through (e.g. flat layout files
        // pre-dating the encoder rules).
        all.extend(
            sess.list_top_level_sessions_with_title()
                .into_iter()
                .filter_map(|(id, count, title)| {
                    let chat_id = id.strip_prefix(&prefix)?;
                    if is_internal_api_session_id(chat_id) {
                        return None;
                    }
                    Some(SessionInfo {
                        id: chat_id.to_string(),
                        message_count: count,
                        title,
                    })
                }),
        );
    }

    // Also fetch from gateway if available.
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let proxy_resp = super::webhook_proxy::api_get_proxy(&state, port, "/sessions").await;
        if proxy_resp.status().is_success() {
            if let Ok(body) = axum::body::to_bytes(proxy_resp.into_body(), 10 * 1024 * 1024).await {
                if let Ok(gateway_sessions) = serde_json::from_slice::<Vec<SessionInfo>>(&body) {
                    // Merge, dedup by id (standalone wins)
                    let existing: std::collections::HashSet<String> =
                        all.iter().map(|s| s.id.clone()).collect();
                    all.extend(gateway_sessions.into_iter().filter(|s| {
                        !existing.contains(&s.id) && !is_internal_api_session_id(&s.id)
                    }));
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
    #[serde(default)]
    pub since_seq: Option<usize>,
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Deserialize)]
pub struct TopicQueryParams {
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Deserialize)]
pub struct SessionEventStreamQueryParams {
    #[serde(default)]
    pub since_seq: Option<usize>,
    #[serde(default)]
    pub topic: Option<String>,
}

fn default_page_limit() -> usize {
    100
}

fn standalone_api_session_key_with_topic(
    state: &AppState,
    headers: &HeaderMap,
    session_id: &str,
    topic: Option<&str>,
) -> SessionKey {
    let profile_id = api_profile_id_from_headers(state, headers);
    SessionKey::with_profile_topic(&profile_id, "api", session_id, topic.unwrap_or_default())
}

fn append_topic_query(path: &mut String, topic: Option<&str>) {
    if let Some(topic) = topic.filter(|value| !value.is_empty()) {
        path.push_str(if path.contains('?') {
            "&topic="
        } else {
            "?topic="
        });
        path.push_str(&octos_bus::session::encode_path_component(topic));
    }
}

fn append_since_seq_query(path: &mut String, since_seq: Option<usize>) {
    if let Some(since_seq) = since_seq {
        path.push_str(if path.contains('?') {
            "&since_seq="
        } else {
            "?since_seq="
        });
        path.push_str(&since_seq.to_string());
    }
}

fn session_messages_proxy_path(
    id: &str,
    limit: usize,
    offset: usize,
    source: Option<&str>,
    since_seq: Option<usize>,
    topic: Option<&str>,
) -> String {
    let encoded_id = encode_api_session_path_id(id);
    let mut path = format!("/sessions/{encoded_id}/messages?limit={limit}&offset={offset}");
    if let Some(source) = source {
        path.push_str("&source=");
        path.push_str(source);
    }
    if let Some(since_seq) = since_seq {
        path.push_str("&since_seq=");
        path.push_str(&since_seq.to_string());
    }
    append_topic_query(&mut path, topic);
    path
}

pub async fn session_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<PaginationParams>,
) -> Response {
    let limit = params.limit.min(500);
    let offset = params.offset.min(10_000);

    // source=full: always proxy to gateway, which owns the canonical JSONL history.
    let use_full = params.source.as_deref() == Some("full");

    // Try standalone store first in local mode.
    if !use_full {
        if let Some(sessions) = &state.sessions {
            let fetch_count = match offset.checked_add(limit) {
                Some(n) => n,
                None => return (StatusCode::BAD_REQUEST, "invalid pagination").into_response(),
            };
            let key = standalone_api_session_key_with_topic(
                &state,
                &headers,
                &id,
                params.topic.as_deref(),
            );
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
                    thread_id: m.thread_id.clone(),
                })
                .collect();
            if !messages.is_empty() {
                return Json(messages).into_response();
            }
            // Fall through to gateway if the standalone store has no history.
        }
    } // !use_full

    // Proxy to gateway.
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let path = session_messages_proxy_path(
            &id,
            limit,
            offset,
            if use_full {
                Some("full")
            } else {
                params.source.as_deref()
            },
            params.since_seq,
            params.topic.as_deref(),
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
    /// M8.10 PR #1 thread grouping key. Lets the web client render chat
    /// history as `Vec<Thread>` rather than a flat message list. Omitted
    /// from the JSON when `None` so legacy clients that don't read the
    /// field continue to round-trip cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// GET /api/sessions/:id/status -- check if session has an active task.
pub async fn session_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<TopicQueryParams>,
) -> Response {
    // Proxy to gateway (session actors live there)
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let encoded_id = encode_api_session_path_id(&id);
        let mut path = format!("/sessions/{encoded_id}/status");
        append_topic_query(&mut path, params.topic.as_deref());
        return super::webhook_proxy::api_get_proxy(&state, port, &path).await;
    }

    // Standalone mode — no active task tracking
    Json(serde_json::json!({
        "active": false,
    }))
    .into_response()
}

/// GET /api/sessions/:id/events/stream -- subscribe to committed session events.
pub async fn session_event_stream(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<SessionEventStreamQueryParams>,
) -> Response {
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let encoded_id = encode_api_session_path_id(&id);
        let mut path = format!("/sessions/{encoded_id}/events/stream");
        append_since_seq_query(&mut path, params.since_seq);
        append_topic_query(&mut path, params.topic.as_deref());
        return super::webhook_proxy::api_sse_get_proxy(&state, port, &path).await;
    }

    let replay_complete = serde_json::json!({
        "type": "replay_complete",
        "topic": params.topic,
    })
    .to_string();
    let stream = futures::stream::iter(vec![Ok::<Event, Infallible>(
        Event::default().data(replay_complete),
    )]);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// GET /api/sessions/:id/tasks -- list background tasks for a session.
pub async fn session_tasks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<TopicQueryParams>,
) -> Response {
    // Proxy to gateway (task supervisor lives there)
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let encoded_id = encode_api_session_path_id(&id);
        let mut path = format!("/sessions/{encoded_id}/tasks");
        append_topic_query(&mut path, params.topic.as_deref());
        return super::webhook_proxy::api_get_proxy(&state, port, &path).await;
    }

    // Standalone mode — no background tasks
    Json(serde_json::json!([])).into_response()
}

// ───────── M7.9 / W2 task supervisor: cancel + restart-from-node ─────────

/// `POST /api/tasks/{task_id}/cancel` — forward to
/// [`octos_agent::TaskSupervisor::cancel`]. Returns:
///
/// - `200 OK` `{ "task_id": "...", "status": "cancelled" }` when the
///   task was running/queued and has been transitioned to `Cancelled`.
/// - `404 Not Found` when no supervised task carries that id.
/// - `409 Conflict` when the task is already in a terminal state
///   (`Completed` / `Failed` / `Cancelled`).
/// - `503 Service Unavailable` when the API server has no
///   `task_query_store` wired (standalone mode).
pub async fn cancel_task(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Response {
    // Gateway-mode: forward to the gateway process that owns the supervisor.
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let path = format!("/tasks/{}/cancel", encode_api_session_path_id(&task_id));
        return super::webhook_proxy::api_post_proxy_json(
            &state,
            port,
            &path,
            serde_json::json!({}),
        )
        .await;
    }

    let Some(store) = state.task_query_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "task supervisor not wired in standalone mode",
            })),
        )
            .into_response();
    };

    match store.cancel_task(&task_id) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "task_id": task_id,
                "status": "cancelled",
            })),
        )
            .into_response(),
        Err(octos_agent::TaskCancelError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "task_not_found",
                "task_id": task_id,
            })),
        )
            .into_response(),
        Err(octos_agent::TaskCancelError::AlreadyTerminal) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "task_already_terminal",
                "task_id": task_id,
            })),
        )
            .into_response(),
    }
}

/// Body of `POST /api/tasks/{task_id}/restart-from-node`.
#[derive(Debug, Default, Deserialize)]
pub struct RestartFromNodeRequest {
    /// Optional DOT-graph node id to restart from. Upstream cached
    /// outputs from preceding nodes are preserved by the runtime; only
    /// the target node and its downstream subtree re-run.
    #[serde(default)]
    pub node_id: Option<String>,
}

/// `POST /api/tasks/{task_id}/restart-from-node` — forward to
/// [`octos_agent::TaskSupervisor::relaunch`]. Returns:
///
/// - `200 OK` `{ "original_task_id": "...", "new_task_id": "...",
///   "from_node": "..." }` on accept.
/// - `404 Not Found` when the task id is unknown.
/// - `409 Conflict` when the task is still active (callers must cancel
///   first).
/// - `503 Service Unavailable` when no supervisor is wired.
pub async fn restart_task_from_node(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    body: Option<Json<RestartFromNodeRequest>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();

    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let path = format!(
            "/tasks/{}/restart-from-node",
            encode_api_session_path_id(&task_id)
        );
        let proxied_body = serde_json::json!({
            "node_id": body.node_id,
        });
        return super::webhook_proxy::api_post_proxy_json(&state, port, &path, proxied_body).await;
    }

    let Some(store) = state.task_query_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "task supervisor not wired in standalone mode",
            })),
        )
            .into_response();
    };

    let opts = octos_agent::RelaunchOpts {
        from_node: body.node_id.clone(),
    };
    match store.relaunch_task(&task_id, opts) {
        Ok(new_task_id) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "original_task_id": task_id,
                "new_task_id": new_task_id,
                "from_node": body.node_id,
            })),
        )
            .into_response(),
        Err(octos_agent::TaskRelaunchError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "task_not_found",
                "task_id": task_id,
            })),
        )
            .into_response(),
        Err(octos_agent::TaskRelaunchError::StillActive) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "task_still_active",
                "task_id": task_id,
            })),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
pub struct SessionFileInfo {
    pub filename: String,
    pub path: String,
    pub size_bytes: u64,
    pub modified_at: String,
}

fn collect_session_files(
    root: &std::path::Path,
    data_dir: &std::path::Path,
    out: &mut Vec<SessionFileInfo>,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        if metadata.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            collect_session_files(&path, data_dir, out);
            continue;
        }

        if !metadata.is_file() {
            continue;
        }

        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let Some(handle) = response_path_for_profile_file(data_dir, &path) else {
            continue;
        };
        out.push(SessionFileInfo {
            filename,
            path: handle,
            size_bytes: metadata.len(),
            modified_at: modified_rfc3339(&metadata),
        });
    }
}

pub async fn session_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let data_dir = if let Some(sessions) = &state.sessions {
        let sess = sessions.lock().await;
        sess.data_dir()
    } else {
        let identity = identity.as_ref().map(|ext| &ext.0);
        match resolve_profile_data_dir(&state, &headers, identity).await {
            Ok(data_dir) => data_dir,
            Err(response) => return response,
        }
    };

    let mut files = Vec::new();
    for workspace in api_session_workspace_dirs(&data_dir, &id) {
        if workspace.exists() {
            collect_session_files(&workspace, &data_dir, &mut files);
        }
    }

    files.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.path.cmp(&b.path))
    });
    files.dedup_by(|left, right| left.path == right.path);
    Json(files).into_response()
}

pub async fn session_workspace_contract(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let data_dir = if let Some(sessions) = &state.sessions {
        let sess = sessions.lock().await;
        sess.data_dir()
    } else {
        let identity = identity.as_ref().map(|ext| &ext.0);
        match resolve_profile_data_dir(&state, &headers, identity).await {
            Ok(data_dir) => data_dir,
            Err(response) => return response,
        }
    };

    let mut statuses = Vec::new();
    for workspace in api_session_workspace_dirs(&data_dir, &id) {
        if !workspace.exists() {
            continue;
        }
        let Ok(repos) = octos_agent::list_workspace_repos(&workspace) else {
            continue;
        };
        statuses.extend(repos.iter().map(inspect_workspace_contract));
    }

    statuses.sort_by(|left, right| left.repo_label.cmp(&right.repo_label));
    statuses.dedup_by(|left, right| left.repo_label == right.repo_label);
    Json(statuses).into_response()
}

async fn resolve_file_access_data_dir(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> Result<std::path::PathBuf, Response> {
    if should_resolve_file_access_from_profile(headers, identity) {
        match resolve_profile_data_dir(state, headers, identity).await {
            Ok(data_dir) => return Ok(data_dir),
            Err(response) if response.status() != StatusCode::SERVICE_UNAVAILABLE => {
                return Err(response);
            }
            Err(_) => {}
        }
    }

    if let Some(sessions) = &state.sessions {
        let sess = sessions.lock().await;
        return Ok(sess.data_dir());
    }

    resolve_profile_data_dir(state, headers, identity).await
}

fn should_resolve_file_access_from_profile(
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> bool {
    // Hosted/profile-scoped requests must use the same root as /api/files/list so
    // profile-encoded file handles round-trip through /api/files unchanged.
    request_host(headers).is_some_and(|host| !is_local_request_host(&host))
        || headers
            .get("x-profile-id")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
        || identity.is_some()
}

/// PATCH /api/sessions/:id/title — set a manual title for a session.
///
/// Body: `{ "title": "New display title" }`. The title persists across new
/// messages (auto-derivation from first user message no longer overrides it).
/// Empty body or missing field returns 400.
#[derive(Deserialize)]
pub struct UpdateTitleRequest {
    pub title: String,
}

pub async fn update_session_title(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<UpdateTitleRequest>,
) -> Response {
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title must not be empty").into_response();
    }
    if title.chars().count() > 200 {
        return (StatusCode::BAD_REQUEST, "title must be at most 200 chars").into_response();
    }

    let mut updated = false;

    if let Some(sessions) = &state.sessions {
        let mut sess = sessions.lock().await;
        for key in standalone_api_session_key_candidates(&state, &headers, &id) {
            if sess.load(&key).await.is_some() {
                if let Err(e) = sess.update_title(&key, title.clone()).await {
                    tracing::error!(
                        session_key = %key,
                        error = %e,
                        "update_title in standalone store failed"
                    );
                } else {
                    updated = true;
                }
            }
        }
    }

    // Proxy to gateway too, since the session may live in the per-profile
    // SessionManager rather than the serve-process store.
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let path = format!("/sessions/{}/title", encode_api_session_path_id(&id));
        let body_json = serde_json::json!({ "title": title }).to_string();
        let _ = super::webhook_proxy::api_patch_proxy(&state, port, &path, body_json).await;
        // Treat gateway proxy success as also updating; we don't strictly
        // need to inspect the response since the gateway is authoritative
        // when serve has no in-memory copy.
        updated = true;
    }

    if updated {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "session not found").into_response()
    }
}

/// DELETE /api/sessions/:id -- delete a session.
pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    // Clear from the standalone store if available.
    if let Some(sessions) = &state.sessions {
        let mut sess = sessions.lock().await;
        for key in standalone_api_session_key_candidates(&state, &headers, &id) {
            if sess.load(&key).await.is_some() {
                if let Err(e) = sess.clear(&key).await {
                    tracing::error!(
                        session_key = %key,
                        error = %e,
                        "delete session from standalone store failed"
                    );
                }
            }
        }
    }

    // Also proxy delete to gateway — sessions may live in the gateway's
    // SessionManager (per-profile data dir), not just the serve process's store.
    if let Some((_profile_id, port)) = resolve_api_port(&state, &headers).await {
        let path = format!("/sessions/{}", encode_api_session_path_id(&id));
        let _ = super::webhook_proxy::api_delete_proxy(&state, port, &path).await;
    }

    StatusCode::NO_CONTENT.into_response()
}

/// POST /api/upload -- upload files, returns paths for use in /api/chat media field.
///
/// Accepts multipart/form-data with one or more `file` fields.
/// Returns JSON array of server-side upload handles.
pub async fn upload(
    State(_state): State<Arc<AppState>>,
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
        let handle = encode_tmp_upload_handle(&dest, Some(&safe_name)).ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encode upload handle".into(),
        ))?;
        paths.push(handle);
    }

    if paths.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no files in request".into()));
    }

    Ok(Json(paths))
}

/// POST /api/site-files/upload -- upload files directly into a site workspace.
///
/// Accepts multipart/form-data with:
/// - `session_id` (text)
/// - `site_slug` (text)
/// - `target_dir` (optional text, defaults by template)
/// - one or more `file` fields
pub async fn upload_site_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Vec<ContentFileEntry>>, (StatusCode, String)> {
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = resolve_profile_data_dir(&state, &headers, identity)
        .await
        .map_err(|response| {
            (
                response.status(),
                "failed to resolve profile data dir".into(),
            )
        })?;

    let mut session_id: Option<String> = None;
    let mut site_slug: Option<String> = None;
    let mut target_dir: Option<String> = None;
    let mut uploads: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    let mut total_size: u64 = 0;
    const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;
    const MAX_TOTAL_SIZE: u64 = 100 * 1024 * 1024;

    while let Ok(Some(field)) = multipart.next_field().await {
        let field_name = field.name().unwrap_or_default().to_string();
        if let Some(filename) = field.file_name() {
            if field_name != "file" {
                let _ = field.bytes().await;
                continue;
            }

            let filename = filename.to_string();
            if !seen_names.insert(filename.clone()) {
                let _ = field.bytes().await;
                continue;
            }

            let data = field.bytes().await.map_err(|error| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("failed to read uploaded file: {error}"),
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

            uploads.push((filename, data.to_vec()));
            continue;
        }

        let value = field.text().await.map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                format!("failed to read form field `{field_name}`: {error}"),
            )
        })?;
        let value = value.trim().to_string();
        match field_name.as_str() {
            "session_id" if !value.is_empty() => session_id = Some(value),
            "site_slug" if !value.is_empty() => site_slug = Some(value),
            "target_dir" if !value.is_empty() => target_dir = Some(value),
            _ => {}
        }
    }

    if uploads.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no files in request".into()));
    }

    let session_id = session_id.ok_or((StatusCode::BAD_REQUEST, "missing session_id".into()))?;
    let site_slug = site_slug.ok_or((StatusCode::BAD_REQUEST, "missing site_slug".into()))?;

    let project_dir = api_session_workspace_dirs(&data_dir, &session_id)
        .into_iter()
        .map(|workspace| workspace.join("sites").join(&site_slug))
        .find(|candidate| candidate.exists())
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("site workspace not found for session `{session_id}` and `{site_slug}`"),
        ))?;

    let metadata = read_site_project_metadata(&project_dir);
    let requested_target =
        target_dir.unwrap_or_else(|| default_site_upload_dir(metadata.as_ref()).to_string());
    let target_relative = safe_relative_subdir(&requested_target)
        .ok_or((StatusCode::BAD_REQUEST, "invalid target_dir".into()))?;
    let destination_dir = project_dir.join(&target_relative);
    tokio::fs::create_dir_all(&destination_dir)
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to create destination directory: {error}"),
            )
        })?;

    let group_root = format!("sites/{site_slug}");
    let group = if target_relative.as_os_str().is_empty() {
        group_root.clone()
    } else {
        format!(
            "{group_root}/{}",
            target_relative.to_string_lossy().replace('\\', "/")
        )
    };

    let mut saved = Vec::new();
    for (filename, data) in uploads {
        let safe_name = sanitize_upload_filename(&filename);
        let destination = dedupe_destination(&destination_dir, &safe_name);
        tokio::fs::write(&destination, &data)
            .await
            .map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to write uploaded file: {error}"),
                )
            })?;

        let meta = std::fs::metadata(&destination).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to stat uploaded file: {error}"),
            )
        })?;

        let saved_name = destination
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(&safe_name)
            .to_string();
        let Some(handle) = response_path_for_profile_file(&data_dir, &destination) else {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to encode uploaded file handle".into(),
            ));
        };

        saved.push(ContentFileEntry {
            filename: saved_name.clone(),
            path: handle,
            size: meta.len(),
            modified: modified_rfc3339(&meta),
            category: categorize(&saved_name),
            group: group.clone(),
        });
    }

    Ok(Json(saved))
}

/// GET /api/files?path=... -- serve files by query parameter (for absolute paths).
pub async fn serve_file_by_query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(filename) = params.get("path") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = match resolve_file_access_data_dir(&state, &headers, identity).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    serve_file_impl(&data_dir, filename).await
}

/// GET /api/files/:filename -- serve uploaded files and pipeline report files.
pub async fn serve_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> Response {
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = match resolve_file_access_data_dir(&state, &headers, identity).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    serve_file_impl(&data_dir, &filename).await
}

async fn serve_file_impl(data_dir: &std::path::Path, filename: &str) -> Response {
    let Some(path) = resolve_scoped_download_path(data_dir, filename) else {
        return (StatusCode::FORBIDDEN, "access denied").into_response();
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

fn api_session_workspace_dirs(
    data_dir: &std::path::Path,
    session_id: &str,
) -> Vec<std::path::PathBuf> {
    let profile_id = infer_profile_id_from_data_dir(data_dir);
    let mut dirs = Vec::with_capacity(3);
    let mut seen = HashSet::new();

    for key in [
        SessionKey::with_profile(&profile_id, "api", session_id),
        SessionKey::with_profile(MAIN_PROFILE_ID, "api", session_id),
        SessionKey::new("api", session_id),
    ] {
        let encoded_base = octos_bus::session::encode_path_component(key.base_key());
        let path = data_dir.join("users").join(encoded_base).join("workspace");
        if seen.insert(path.clone()) {
            dirs.push(path);
        }
    }

    dirs
}

#[cfg(test)]
fn api_session_workspace_dir(data_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    api_session_workspace_dirs(data_dir, session_id)
        .into_iter()
        .next()
        .unwrap_or_else(|| data_dir.join("users").join("workspace"))
}

fn infer_profile_id_from_data_dir(data_dir: &std::path::Path) -> String {
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

async fn resolve_profile_data_dir(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> Result<std::path::PathBuf, Response> {
    if let Some((_profile_id, _port)) = resolve_api_port(state, headers).await {
        if let Some(ref ps) = state.profile_store {
            let header_profile_id = routed_profile_id_from_headers(state, headers);
            let identity_profile_id = match identity {
                Some(AuthIdentity::User { id, .. }) => Some(id.as_str()),
                Some(AuthIdentity::Admin) => Some(ADMIN_PROFILE_ID),
                None => None,
            };

            if let Some(pid) = header_profile_id.as_deref().or(identity_profile_id) {
                match ps.get(pid) {
                    Ok(Some(profile)) => return Ok(ps.resolve_data_dir(&profile)),
                    Ok(None) => {
                        return Err((StatusCode::NOT_FOUND, "profile not found").into_response());
                    }
                    Err(error) => {
                        return Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("profile lookup failed: {error}"),
                        )
                            .into_response());
                    }
                }
            }
            return Err((
                StatusCode::BAD_REQUEST,
                "missing X-Profile-Id and no authenticated profile context",
            )
                .into_response());
        }
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no profile store").into_response());
    }

    Err((StatusCode::SERVICE_UNAVAILABLE, "no gateway").into_response())
}

fn resolve_profile_data_dir_by_id(
    state: &AppState,
    profile_id: &str,
) -> Result<std::path::PathBuf, Response> {
    let profile_id = if profile_id.is_empty() {
        MAIN_PROFILE_ID
    } else {
        profile_id
    };

    let Some(ref store) = state.profile_store else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no profile store").into_response());
    };

    match store.get(profile_id) {
        Ok(Some(profile)) => Ok(store.resolve_data_dir(&profile)),
        Ok(None) => Err((StatusCode::NOT_FOUND, "profile not found").into_response()),
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("profile lookup failed: {error}"),
        )
            .into_response()),
    }
}

fn sanitize_upload_filename(filename: &str) -> String {
    filename
        .replace(['/', '\\', '\0'], "_")
        .chars()
        .take(200)
        .collect::<String>()
}

fn safe_relative_subdir(dir: &str) -> Option<std::path::PathBuf> {
    let normalized = dir.trim().replace('\\', "/");
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return Some(std::path::PathBuf::new());
    }

    let mut relative = std::path::PathBuf::new();
    for component in std::path::Path::new(trimmed).components() {
        match component {
            std::path::Component::Normal(segment) => relative.push(segment),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }

    Some(relative)
}

fn default_site_upload_dir(metadata: Option<&SiteProjectMetadata>) -> &'static str {
    match metadata.map(|meta| meta.template.as_str()) {
        Some("quarto-lesson") => "images/uploads",
        _ => "public/uploads",
    }
}

fn dedupe_destination(dest_dir: &std::path::Path, filename: &str) -> std::path::PathBuf {
    let candidate = dest_dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    let stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("file");
    let extension = std::path::Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty());

    for index in 2..10_000 {
        let deduped = match extension {
            Some(extension) => format!("{stem}-{index}.{extension}"),
            None => format!("{stem}-{index}"),
        };
        let deduped_path = dest_dir.join(&deduped);
        if !deduped_path.exists() {
            return deduped_path;
        }
    }

    candidate
}

fn modified_rfc3339(meta: &std::fs::Metadata) -> String {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

fn site_preview_html(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>{title}</title>
    <style>
      :root {{
        color-scheme: light dark;
        --bg: #0f172a;
        --panel: rgba(15, 23, 42, 0.78);
        --text: #e2e8f0;
        --muted: #94a3b8;
        --border: rgba(148, 163, 184, 0.18);
        --accent: #38bdf8;
        --font: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      }}
      * {{ box-sizing: border-box; }}
      body {{
        margin: 0;
        min-height: 100vh;
        display: grid;
        place-items: center;
        padding: 24px;
        color: var(--text);
        font-family: var(--font);
        background:
          radial-gradient(circle at top right, rgba(56, 189, 248, 0.18), transparent 22rem),
          linear-gradient(180deg, #0f172a 0%, #111827 100%);
      }}
      .card {{
        width: min(820px, 100%);
        padding: 24px;
        border: 1px solid var(--border);
        border-radius: 24px;
        background: var(--panel);
        backdrop-filter: blur(18px);
      }}
      h1 {{
        margin: 0 0 12px;
        font-size: clamp(1.6rem, 3vw, 2.4rem);
        letter-spacing: -0.04em;
      }}
      p {{
        margin: 0;
        line-height: 1.75;
        color: var(--muted);
        white-space: pre-wrap;
      }}
      code {{
        color: var(--text);
      }}
    </style>
  </head>
  <body>
    <article class="card">
      <h1>{title}</h1>
      <p>{body}</p>
    </article>
  </body>
</html>"#,
        title = title,
        body = body,
    );

    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

fn preview_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs" | "cjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn output_dir_for_site(
    project_dir: &std::path::Path,
    metadata: &SiteProjectMetadata,
) -> std::path::PathBuf {
    project_dir.join(&metadata.build_output_dir)
}

fn newest_tree_mtime(
    root: &std::path::Path,
    skip_dir_names: &[&str],
) -> Option<std::time::SystemTime> {
    fn walk(
        dir: &std::path::Path,
        skip_dir_names: &[&str],
        latest: &mut Option<std::time::SystemTime>,
    ) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if path.is_dir() {
                if skip_dir_names.iter().any(|skip| *skip == file_name) {
                    continue;
                }
                walk(&path, skip_dir_names, latest);
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if latest.map(|current| modified > current).unwrap_or(true) {
                        *latest = Some(modified);
                    }
                }
            }
        }
    }

    if root.is_file() {
        return root.metadata().ok()?.modified().ok();
    }

    let mut latest = None;
    walk(root, skip_dir_names, &mut latest);
    latest
}

fn site_build_needed(project_dir: &std::path::Path, output_dir: &std::path::Path) -> bool {
    if !output_dir.exists() {
        return true;
    }

    let output_time = newest_tree_mtime(output_dir, &[]);
    let source_time = newest_tree_mtime(
        project_dir,
        &[
            "node_modules",
            ".git",
            ".next",
            ".astro",
            "dist",
            "out",
            "docs",
        ],
    );

    match (source_time, output_time) {
        (Some(source_time), Some(output_time)) => source_time > output_time,
        (Some(_), None) => true,
        _ => false,
    }
}

fn site_build_cache_dir(project_dir: &std::path::Path) -> std::path::PathBuf {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let project_key = project_dir
        .to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    let preferred = std::env::temp_dir()
        .join("octos-site-build-npm-cache")
        .join(user)
        .join(project_key);
    let _ = std::fs::create_dir_all(&preferred);
    preferred
}

fn apply_site_build_env(command: &mut std::process::Command, project_dir: &std::path::Path) {
    let cache_dir = site_build_cache_dir(project_dir);
    command
        .env("ASTRO_TELEMETRY_DISABLED", "1")
        .env("NPM_CONFIG_CACHE", &cache_dir)
        .env("npm_config_cache", &cache_dir);
}

fn run_build_command(command: &mut std::process::Command, label: &str) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|e| format!("{label} failed to start: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = format!("{stdout}\n{stderr}").trim().to_string();
    if detail.is_empty() {
        Err(format!("{label} failed with status {}", output.status))
    } else {
        Err(format!("{label} failed:\n{detail}"))
    }
}

fn ensure_site_build_output(
    project_dir: &std::path::Path,
    metadata: &SiteProjectMetadata,
) -> Result<std::path::PathBuf, String> {
    fn site_build_locks() -> &'static Mutex<HashMap<String, Arc<Mutex<()>>>> {
        static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
        LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn site_build_lock(project_dir: &std::path::Path) -> Arc<Mutex<()>> {
        let key = std::fs::canonicalize(project_dir)
            .unwrap_or_else(|_| project_dir.to_path_buf())
            .to_string_lossy()
            .to_string();
        let mut locks = site_build_locks().lock().unwrap();
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    let build_lock = site_build_lock(project_dir);
    let _build_guard = build_lock.lock().unwrap();
    let output_dir = output_dir_for_site(project_dir, metadata);
    if !site_build_needed(project_dir, &output_dir) {
        return Ok(output_dir);
    }

    match metadata.template.as_str() {
        "quarto-lesson" => {
            let mut render = std::process::Command::new("quarto");
            render.current_dir(project_dir).arg("render");
            run_build_command(&mut render, "quarto render")?;
        }
        "astro-site" | "nextjs-app" | "react-vite" => {
            if !project_dir.join("node_modules").exists() {
                let mut install = std::process::Command::new("npm");
                install.current_dir(project_dir).arg("install");
                apply_site_build_env(&mut install, project_dir);
                run_build_command(&mut install, "npm install")?;
            }
            let mut build = std::process::Command::new("npm");
            build.current_dir(project_dir).arg("run").arg("build");
            apply_site_build_env(&mut build, project_dir);
            run_build_command(&mut build, "npm run build")?;
        }
        other => return Err(format!("unsupported site template: {other}")),
    }

    if !output_dir.exists() {
        return Err(format!(
            "site build completed but {} was not created",
            output_dir.display()
        ));
    }

    Ok(output_dir)
}

fn safe_preview_join(root: &std::path::Path, request_path: &str) -> Option<std::path::PathBuf> {
    let mut joined = root.to_path_buf();
    for segment in request_path.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." || segment.contains('\\') {
            return None;
        }
        joined.push(segment);
    }
    Some(joined)
}

fn resolve_preview_asset_path(
    output_dir: &std::path::Path,
    request_path: &str,
) -> Option<std::path::PathBuf> {
    fn resolve_direct(
        output_dir: &std::path::Path,
        request_path: &str,
    ) -> Option<std::path::PathBuf> {
        let candidate = safe_preview_join(output_dir, request_path)?;
        if request_path.is_empty() {
            Some(output_dir.join("index.html"))
        } else if candidate.is_dir() {
            Some(candidate.join("index.html"))
        } else if candidate.exists() {
            Some(candidate)
        } else {
            let nested_index = candidate.join("index.html");
            if nested_index.exists() {
                Some(nested_index)
            } else if !request_path.contains('.') {
                let html = candidate.with_extension("html");
                if html.exists() { Some(html) } else { None }
            } else {
                None
            }
        }
    }

    let request_path = request_path.trim_start_matches('/');
    let resolved = resolve_direct(output_dir, request_path).or_else(|| {
        if request_path.contains('.') {
            return None;
        }

        let segments: Vec<&str> = request_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        if segments.len() < 2 {
            return None;
        }

        // Legacy generated sites sometimes emit page-relative links such as
        // "./capabilities/" from "/concepts/". When that happens, the
        // browser requests "concepts/capabilities/". Fall back to the
        // rightmost route segment at the preview root if it exists there.
        for start in 1..segments.len() {
            let fallback_path = segments[start..].join("/");
            if let Some(path) = resolve_direct(output_dir, &fallback_path) {
                return Some(path);
            }
        }

        None
    })?;

    let canonical_root = std::fs::canonicalize(output_dir).ok()?;
    let canonical_resolved = std::fs::canonicalize(resolved).ok()?;
    if !canonical_resolved.starts_with(&canonical_root) {
        return None;
    }

    Some(canonical_resolved)
}

async fn serve_preview_file(path: std::path::PathBuf) -> Response {
    let data = match tokio::fs::read(&path).await {
        Ok(data) => data,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let cache_control = if path.extension().and_then(|ext| ext.to_str()) == Some("html") {
        "no-cache, no-store, must-revalidate"
    } else {
        "public, max-age=30"
    };

    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                preview_content_type(&path),
            ),
            (axum::http::header::CACHE_CONTROL, cache_control),
        ],
        data,
    )
        .into_response()
}

async fn serve_site_preview_impl(
    data_dir: std::path::PathBuf,
    session_id: String,
    site_slug: String,
    request_path: String,
) -> Response {
    let project_dir = api_session_workspace_dirs(&data_dir, &session_id)
        .into_iter()
        .map(|workspace| workspace.join("sites").join(&site_slug))
        .find(|candidate| candidate.exists());

    let Some(project_dir) = project_dir else {
        return site_preview_html(
            StatusCode::NOT_FOUND,
            "Site Preview Not Found",
            &format!(
                "No scaffold exists yet for session `{session_id}` and site `{site_slug}`.\n\nCreate the site session first so Octos can scaffold the project workspace."
            ),
        );
    };

    let Some(metadata) = read_site_project_metadata(&project_dir) else {
        return site_preview_html(
            StatusCode::NOT_FOUND,
            "Missing Site Metadata",
            &format!(
                "The project exists at `{}` but `{}` is missing or invalid.",
                project_dir.display(),
                "mofa-site-session.json",
            ),
        );
    };

    let build_task = {
        let project_dir = project_dir.clone();
        let metadata = metadata.clone();
        tokio::task::spawn_blocking(move || ensure_site_build_output(&project_dir, &metadata))
    };

    let output_dir = match build_task.await {
        Ok(Ok(output_dir)) => output_dir,
        Ok(Err(error)) => {
            return site_preview_html(
                StatusCode::OK,
                "Preview Build Failed",
                &format!(
                    "Octos could not build the preview for `{}`.\n\n{}",
                    metadata.template, error
                ),
            );
        }
        Err(error) => {
            return site_preview_html(
                StatusCode::OK,
                "Preview Build Failed",
                &format!("The preview worker crashed: {error}"),
            );
        }
    };

    let Some(path) = resolve_preview_asset_path(&output_dir, &request_path) else {
        return site_preview_html(
            StatusCode::NOT_FOUND,
            "Preview Asset Missing",
            &format!(
                "The built preview exists, but `{}` was not found under `{}`.",
                request_path,
                output_dir.display(),
            ),
        );
    };

    serve_preview_file(path).await
}

/// GET /api/site-preview/{session_id}/{site_slug} — serve the preview root for a site session.
pub async fn serve_site_preview_root(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path((session_id, site_slug)): axum::extract::Path<(String, String)>,
) -> Response {
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = match resolve_profile_data_dir(&state, &headers, identity).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    serve_site_preview_impl(data_dir, session_id, site_slug, String::new()).await
}

/// GET /api/site-preview/{session_id}/{site_slug}/{*path} — serve built preview assets.
pub async fn serve_site_preview_path(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path((session_id, site_slug, request_path)): axum::extract::Path<(
        String,
        String,
        String,
    )>,
) -> Response {
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = match resolve_profile_data_dir(&state, &headers, identity).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    serve_site_preview_impl(data_dir, session_id, site_slug, request_path).await
}

/// GET /api/preview/{profile_id}/{session_id}/{site_slug} — public preview root for site iframes.
pub async fn serve_public_site_preview_root(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((profile_id, session_id, site_slug)): axum::extract::Path<(
        String,
        String,
        String,
    )>,
) -> Response {
    let data_dir = match resolve_profile_data_dir_by_id(&state, &profile_id) {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    serve_site_preview_impl(data_dir, session_id, site_slug, String::new()).await
}

/// GET /api/preview/{profile_id}/{session_id}/{site_slug}/{*path} — public preview assets.
pub async fn serve_public_site_preview_path(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((profile_id, session_id, site_slug, request_path)): axum::extract::Path<(
        String,
        String,
        String,
        String,
    )>,
) -> Response {
    let data_dir = match resolve_profile_data_dir_by_id(&state, &profile_id) {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    serve_site_preview_impl(data_dir, session_id, site_slug, request_path).await
}

/// GET /api/files/list?dirs=research,slides,skill-output&session_id=... — list files in profile content directories.
fn should_skip_listing_dir(dir_name: &str, include_build: bool) -> bool {
    let lower = dir_name.to_ascii_lowercase();
    lower.starts_with('.')
        || matches!(lower.as_str(), "node_modules" | "coverage" | "target")
        || lower == "output_old"
        || (!include_build && matches!(lower.as_str(), "dist" | "out" | "docs" | "build"))
}

pub async fn list_content_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = match resolve_profile_data_dir(&state, &headers, identity).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };

    let dirs_param = params
        .get("dirs")
        .cloned()
        .unwrap_or_else(|| "research,slides,skill-output".to_string());
    let requested_dirs: Vec<String> = dirs_param
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let session_id = params
        .get("session_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let include_build = params
        .get("include_build")
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let session_scoped = session_id.is_some();

    let mut scan_dirs = requested_dirs.clone();
    if let Some(session_id) = session_id {
        scan_dirs.clear();
        for workspace in api_session_workspace_dirs(&data_dir, session_id) {
            if workspace.exists() {
                for dir_name in &requested_dirs {
                    if std::path::Path::new(dir_name.as_str()).is_absolute() {
                        continue;
                    }
                    let ws_dir = workspace.join(dir_name);
                    if ws_dir.exists() && ws_dir.is_dir() {
                        scan_dirs.push(ws_dir.to_string_lossy().to_string());
                    }
                }
            }
        }
    } else {
        // Scan per-user workspace directories that match the requested dirs.
        // Only use original relative dir names — absolute paths from prior
        // iterations would bypass ws.join() (Path::join replaces on absolute).
        let users_dir = data_dir.join("users");
        if let Ok(entries) = std::fs::read_dir(&users_dir) {
            for entry in entries.flatten() {
                let ws = entry.path().join("workspace");
                if !ws.exists() {
                    continue;
                }
                for dir_name in &requested_dirs {
                    if std::path::Path::new(dir_name.as_str()).is_absolute() {
                        continue;
                    }
                    let ws_dir = ws.join(dir_name);
                    if ws_dir.exists() && ws_dir.is_dir() {
                        scan_dirs.push(ws_dir.to_string_lossy().to_string());
                    }
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

    fn display_dir_for_scan(dir_name: &str) -> String {
        if !std::path::Path::new(dir_name).is_absolute() {
            return dir_name.trim_matches('/').to_string();
        }

        let normalized = dir_name.replace('\\', "/");
        if let Some((_, suffix)) = normalized.rsplit_once("/workspace/") {
            let trimmed = suffix.trim_matches('/');
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }

        let path = std::path::Path::new(dir_name);
        let parts: Vec<&str> = path
            .components()
            .rev()
            .take(2)
            .map(|component| component.as_os_str().to_str().unwrap_or(""))
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            "files".into()
        } else {
            parts.into_iter().rev().collect::<Vec<_>>().join("/")
        }
    }

    // Keep meaningful session files while filtering obvious intermediates.
    fn is_output_file(filename: &str) -> bool {
        let lower = filename.to_lowercase();
        // Skip hidden files
        if lower.starts_with('.') {
            return false;
        }
        // Skip research intermediate files
        if lower.starts_with('_') {
            return false;
        } // _report.md, _search_results.md, _sources.json
        // Skip intermediates
        if lower.starts_with("panel-") {
            return false;
        }
        if lower.contains("-ref.") {
            return false;
        } // mofa reference images
        // Only keep meaningful output extensions
        matches!(
            lower.rsplit('.').next().unwrap_or(""),
            "md" | "markdown"
                | "txt"
                | "pptx"
                | "pdf"
                | "docx"
                | "xlsx"
                | "png"
                | "jpg"
                | "jpeg"
                | "webp"
                | "gif"
                | "svg"
                | "avif"
                | "mp3"
                | "wav"
                | "mp4"
                | "js"
                | "jsx"
                | "ts"
                | "tsx"
                | "json"
                | "css"
                | "html"
                | "astro"
                | "qmd"
                | "yaml"
                | "yml"
                | "sh"
                | "mjs"
                | "cjs"
        )
    }

    fn collect_files_recursive(
        data_dir: &std::path::Path,
        current_dir: &std::path::Path,
        display_root: &str,
        relative_dir: &std::path::Path,
        include_build: bool,
        allow_nested_dirs: bool,
        files: &mut Vec<ContentFile>,
    ) {
        let entries = match std::fs::read_dir(current_dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                if !allow_nested_dirs || should_skip_listing_dir(&name, include_build) {
                    continue;
                }
                let mut next_relative = relative_dir.to_path_buf();
                next_relative.push(&name);
                collect_files_recursive(
                    data_dir,
                    &path,
                    display_root,
                    &next_relative,
                    include_build,
                    allow_nested_dirs,
                    files,
                );
                continue;
            }

            if !path.is_file() || !is_output_file(&name) {
                continue;
            }

            let meta = match path.metadata() {
                Ok(meta) => meta,
                Err(_) => continue,
            };

            let group = if relative_dir.as_os_str().is_empty() {
                display_root.to_string()
            } else {
                format!(
                    "{display_root}/{}",
                    relative_dir.to_string_lossy().replace('\\', "/")
                )
            };
            let Some(handle) = response_path_for_profile_file(data_dir, &path) else {
                continue;
            };

            files.push(ContentFile {
                category: categorize(&name),
                filename: name,
                path: handle,
                size: meta.len(),
                modified: modified_rfc3339(&meta),
                group,
            });
        }
    }

    let mut files = Vec::new();
    for dir_name in &scan_dirs {
        let dir_path = if std::path::Path::new(dir_name.as_str()).is_absolute() {
            std::path::PathBuf::from(dir_name.as_str())
        } else {
            data_dir.join(dir_name.as_str())
        };
        if !dir_path.exists() {
            continue;
        }
        let display_dir = display_dir_for_scan(dir_name);
        let allow_nested_dirs = display_dir != "research";
        collect_files_recursive(
            &data_dir,
            &dir_path,
            &display_dir,
            std::path::Path::new(""),
            include_build,
            allow_nested_dirs,
            &mut files,
        );
    }

    // Sort by modified desc; session-scoped project views need a larger ceiling
    // so the source tree remains inspectable.
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    files.truncate(if session_scoped { 1000 } else { 100 });
    Json(files).into_response()
}

fn categorize(filename: &str) -> String {
    let lower = filename.to_lowercase();
    if lower.ends_with(".md")
        || lower.ends_with(".markdown")
        || lower.ends_with(".txt")
        || lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".json")
        || lower.ends_with(".css")
        || lower.ends_with(".html")
        || lower.ends_with(".astro")
        || lower.ends_with(".qmd")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".sh")
        || lower.ends_with(".mjs")
        || lower.ends_with(".cjs")
    {
        "report".into()
    } else if lower.ends_with(".pptx") || lower.ends_with(".pdf") {
        "slides".into()
    } else if lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".webp")
        || lower.ends_with(".gif")
        || lower.ends_with(".svg")
        || lower.ends_with(".avif")
    {
        "image".into()
    } else if lower.ends_with(".mp3") || lower.ends_with(".wav") || lower.ends_with(".ogg") {
        "audio".into()
    } else if lower.ends_with(".mp4") || lower.ends_with(".webm") {
        "video".into()
    } else {
        "other".into()
    }
}

/// GET /api/status -- server status.
#[derive(Serialize)]
pub struct StatusResponse {
    pub version: String,
    pub model: String,
    pub provider: String,
    pub uptime_secs: i64,
    pub agent_configured: bool,
    /// Public-facing base domain this mini serves profiles under
    /// (e.g. `"crew.ominix.io"`, `"bot.ominix.io"`). The dashboard and
    /// octos-web client consume this to render correct preview URLs
    /// and infer profile IDs from hostnames. Always a concrete string
    /// — falls back to `DEFAULT_BASE_DOMAIN` when unconfigured.
    pub base_domain: String,
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
    let base_domain = state
        .base_domain
        .clone()
        .unwrap_or_else(|| crate::api::DEFAULT_BASE_DOMAIN.to_string());
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        model,
        provider,
        uptime_secs: uptime.num_seconds(),
        agent_configured: state.agent.is_some(),
        base_domain,
    })
}

/// GET /api/version — public version endpoint (no auth required).
pub async fn version(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
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
        "tunnel_domain": state.tunnel_domain,
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
                if let Some((profile_id, port)) = resolve_api_port(&state, &headers).await {
                    let ws_tx2 = ws_tx.clone();
                    let _abort_ref = abort_handle.clone();
                    let http_client = state.http_client.clone();
                    let handle = tokio::spawn(async move {
                        ws_proxy_to_gateway(
                            ws_tx2,
                            &http_client,
                            port,
                            Some(&profile_id),
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
                        topic: None,
                        stream: true,
                        media: media.clone(),
                        attach_only: false,
                        client_message_id: None,
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
    profile_id: Option<&str>,
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
        "target_profile_id": profile_id,
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
    let session_key = SessionKey::with_profile(MAIN_PROFILE_ID, "api", session_id);

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
    let session_key2 = SessionKey::with_profile(MAIN_PROFILE_ID, "api", &session_id);

    // Spawn the agent task
    tokio::spawn(async move {
        let result = request_agent
            .process_message(&message, &history, media)
            .await;

        match result {
            Ok(response) => {
                // Save conversation messages to the canonical per-user JSONL.
                // Mirrors `chat_sync` and `chat_streaming`; closes the
                // standalone-serve split-brain by routing through the same
                // helper the gateway-side `ApiChannel` uses. Capture the
                // committed seq of the final assistant message so the
                // WebSocket-bridged `done` event can thread it back to the
                // web client (M8.10-A).
                let assistant_committed_seq: Option<u64> = {
                    let mut last_assistant_seq: Option<u64> = None;
                    for msg in &response.messages {
                        let is_assistant = msg.role == octos_core::MessageRole::Assistant;
                        match persist_chat_message_through_canonical(
                            &sessions,
                            &session_key2,
                            msg.clone(),
                        )
                        .await
                        {
                            Ok(seq) if is_assistant => {
                                last_assistant_seq = u64::try_from(seq).ok();
                            }
                            Ok(_) => {}
                            Err(_) => {}
                        }
                    }
                    last_assistant_seq
                };

                let provider_metadata = response.provider_metadata.clone();
                let model_id = provider_metadata
                    .as_ref()
                    .map(|meta| meta.model.clone())
                    .or_else(|| {
                        let provider = request_agent.llm_provider();
                        let model = provider.model_id();
                        if model.is_empty() {
                            None
                        } else {
                            Some(model.to_string())
                        }
                    });
                let session_cost = model_id.as_deref().and_then(model_pricing).map(|pricing| {
                    pricing.cost(
                        response.token_usage.input_tokens,
                        response.token_usage.output_tokens,
                    )
                });
                let mut done = serde_json::json!({
                    "type": "done",
                    "content": response.content,
                    "model": provider_metadata.as_ref().map(|meta| meta.display_label()),
                    "provider": provider_metadata.as_ref().map(|meta| meta.provider.clone()),
                    "model_id": model_id,
                    "endpoint": provider_metadata.as_ref().and_then(|meta| meta.endpoint.clone()),
                    "tokens_in": response.token_usage.input_tokens,
                    "tokens_out": response.token_usage.output_tokens,
                    "session_cost": session_cost,
                });
                if let Some(seq) = assistant_committed_seq {
                    done["committed_seq"] = serde_json::Value::from(seq);
                }
                // Bug 3 / W1.G4 cost panel — flatten per-node cost rows from
                // tool results' structured side-channel into the SSE done
                // event so the dashboard CostBreakdown panel can render
                // real per-node attribution from `run_pipeline` runs.
                let mut all_node_costs: Vec<serde_json::Value> = Vec::new();
                for (_tool_call_id, meta) in &response.tool_results {
                    if let Some(arr) = meta.get("node_costs").and_then(|v| v.as_array()) {
                        all_node_costs.extend(arr.iter().cloned());
                    }
                }
                if !all_node_costs.is_empty() {
                    done["node_costs"] = serde_json::Value::Array(all_node_costs);
                }
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

    /// FA-12f follow-up: the outer `ChatRequest` (served at `/api/chat`) must
    /// accept `client_message_id` so it survives proxy forwarding to the
    /// gateway. The prior fix patched only the gateway-internal struct; the
    /// outer struct silently dropped the field and overflow replies arrived
    /// with `response_to_client_message_id: null`, breaking web-side
    /// correlation under `/queue speculative`.
    #[test]
    fn chat_request_accepts_client_message_id() {
        let json = r#"{"message": "hi", "client_message_id": "client-bravo-xyz"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.client_message_id.as_deref(), Some("client-bravo-xyz"));
    }

    #[test]
    fn chat_request_client_message_id_defaults_to_none() {
        let json = r#"{"message": "hi"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert!(req.client_message_id.is_none());
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
            title: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["id"], "test-session");
        assert_eq!(json["message_count"], 42);
        assert!(
            json.get("title").is_none(),
            "None title must be omitted from JSON"
        );
    }

    #[test]
    fn session_info_serialize_with_title_includes_field() {
        let info = SessionInfo {
            id: "test-session".into(),
            message_count: 7,
            title: Some("My Pinned Chat".into()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["title"], "My Pinned Chat");
    }

    #[test]
    fn message_info_serialize() {
        let info = MessageInfo {
            role: "user".into(),
            content: "hello".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            thread_id: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
        assert_eq!(json["timestamp"], "2025-01-01T00:00:00Z");
        // None thread_id must be omitted so legacy clients keep round-tripping.
        assert!(json.get("thread_id").is_none());
    }

    #[test]
    fn message_info_serialize_with_thread_id_includes_field() {
        // M8.10 PR #1: when thread_id is populated the messages endpoint
        // must surface it in the JSON so the web client (PR #3) can group
        // messages into threads without a backfill round-trip.
        let info = MessageInfo {
            role: "assistant".into(),
            content: "answer".into(),
            timestamp: "2026-04-26T00:00:00Z".into(),
            thread_id: Some("thread-cmid-1".into()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["thread_id"], "thread-cmid-1");
    }

    #[test]
    fn status_response_serialize() {
        let resp = StatusResponse {
            version: "0.1.0".into(),
            model: "gpt-4".into(),
            provider: "openai".into(),
            uptime_secs: 120,
            agent_configured: true,
            base_domain: "crew.ominix.io".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["version"], "0.1.0");
        assert_eq!(json["model"], "gpt-4");
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["uptime_secs"], 120);
        assert_eq!(json["agent_configured"], true);
        assert_eq!(json["base_domain"], "crew.ominix.io");
    }

    #[tokio::test]
    async fn status_returns_configured_base_domain() {
        let state = Arc::new(crate::api::AppState {
            base_domain: Some("bot.ominix.io".into()),
            ..crate::api::AppState::empty_for_tests()
        });
        let resp = status(State(state)).await;
        assert_eq!(resp.0.base_domain, "bot.ominix.io");
    }

    #[tokio::test]
    async fn status_defaults_base_domain_when_unconfigured() {
        let state = Arc::new(crate::api::AppState {
            base_domain: None,
            ..crate::api::AppState::empty_for_tests()
        });
        let resp = status(State(state)).await;
        // Backward compat: `None` surfaces as the historical `crew.ominix.io`
        // so existing dashboards / web clients keep rendering the right URL
        // until operators opt in to a per-mini value.
        assert_eq!(resp.0.base_domain, "crew.ominix.io");
    }

    #[test]
    fn api_session_workspace_dir_uses_base_session_key() {
        let base = std::path::Path::new("/tmp/octos-data/profiles/dspfac/data");
        let path = api_session_workspace_dir(base, "slides-123");
        assert_eq!(
            path,
            base.join("users")
                .join("dspfac%3Aapi%3Aslides-123")
                .join("workspace")
        );
    }

    #[test]
    fn api_session_workspace_dir_encodes_session_id_safely() {
        let base = std::path::Path::new("/tmp/octos-data/profiles/dspfac/data");
        let path = api_session_workspace_dir(base, "web:abc/123");
        assert_eq!(
            path,
            base.join("users")
                .join("dspfac%3Aapi%3Aweb%3Aabc%2F123")
                .join("workspace")
        );
    }

    #[test]
    fn api_session_workspace_dirs_use_current_profile_scope() {
        let base = std::path::Path::new("/tmp/octos-data/profiles/dspfac/data");
        let dirs = api_session_workspace_dirs(base, "slides-123");

        assert_eq!(dirs.len(), 3);
        assert_eq!(
            dirs[0],
            base.join("users")
                .join("dspfac%3Aapi%3Aslides-123")
                .join("workspace")
        );
        assert_eq!(
            dirs[1],
            base.join("users")
                .join("_main%3Aapi%3Aslides-123")
                .join("workspace")
        );
        assert_eq!(
            dirs[2],
            base.join("users")
                .join("api%3Aslides-123")
                .join("workspace")
        );
    }

    #[test]
    fn encode_api_session_path_id_escapes_reserved_characters() {
        assert_eq!(
            encode_api_session_path_id("web-123#slides topic"),
            "web-123%23slides%20topic"
        );
    }

    #[test]
    fn session_messages_proxy_path_encodes_session_id() {
        let path = session_messages_proxy_path("web-123#slides", 25, 0, None, None, None);
        assert!(path.starts_with("/sessions/web-123%23slides/messages?"));
    }

    #[test]
    fn site_build_cache_dir_prefers_project_local_cache() {
        let project_dir = tempfile::tempdir().unwrap();
        let cache_dir = site_build_cache_dir(project_dir.path());

        assert!(cache_dir.starts_with(std::env::temp_dir()));
        assert!(
            cache_dir
                .to_string_lossy()
                .contains("octos-site-build-npm-cache")
        );
    }

    #[test]
    fn response_path_for_profile_file_hides_absolute_paths() {
        let base = tempfile::tempdir().unwrap();
        let file = base.path().join("slides/demo/output/deck.pptx");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"pptx").unwrap();

        let handle = response_path_for_profile_file(base.path(), &file).expect("handle");

        assert_ne!(handle, file.to_string_lossy());
        assert!(handle.ends_with("/deck.pptx"));
    }

    #[test]
    fn should_resolve_file_access_from_profile_for_remote_host() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("windows.ominix.io"),
        );

        assert!(should_resolve_file_access_from_profile(&headers, None));
    }

    #[test]
    fn should_resolve_file_access_from_profile_skips_local_host_without_identity() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("localhost:8080"),
        );

        assert!(!should_resolve_file_access_from_profile(&headers, None));
    }

    #[tokio::test]
    async fn resolve_file_access_data_dir_falls_back_to_session_store_when_gateway_missing() {
        let data_dir = tempfile::tempdir().unwrap();
        let state = AppState {
            sessions: Some(Arc::new(tokio::sync::Mutex::new(
                octos_bus::SessionManager::open(data_dir.path()).unwrap(),
            ))),
            ..AppState::empty_for_tests()
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("windows.ominix.io"),
        );

        let resolved = resolve_file_access_data_dir(&state, &headers, None)
            .await
            .expect("fallback data dir");

        assert_eq!(resolved, data_dir.path());
    }

    #[test]
    fn resolve_scoped_download_path_denies_other_profile_absolute_path() {
        let current = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_file = other.path().join("secret.txt");
        std::fs::write(&other_file, b"secret").unwrap();

        assert!(
            resolve_scoped_download_path(current.path(), &other_file.to_string_lossy()).is_none()
        );
    }

    #[test]
    fn resolve_preview_asset_path_falls_back_to_root_route_for_legacy_relative_links() {
        let base = std::env::temp_dir().join(format!(
            "octos-preview-fallback-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join("capabilities")).unwrap();
        std::fs::write(base.join("capabilities").join("index.html"), "ok").unwrap();

        let resolved = resolve_preview_asset_path(&base, "concepts/capabilities/").unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(base.join("capabilities").join("index.html")).unwrap()
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn resolve_preview_asset_path_does_not_fallback_for_missing_assets() {
        let base = std::env::temp_dir().join(format!(
            "octos-preview-fallback-missing-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();

        let resolved = resolve_preview_asset_path(&base, "concepts/missing/");
        assert!(resolved.is_none());

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn site_file_listing_hides_build_dirs_by_default() {
        assert!(should_skip_listing_dir("dist", false));
        assert!(should_skip_listing_dir("out", false));
        assert!(should_skip_listing_dir("docs", false));
        assert!(should_skip_listing_dir("build", false));
        assert!(should_skip_listing_dir("output_old", false));
        assert!(should_skip_listing_dir("node_modules", false));
        assert!(should_skip_listing_dir(".cache", false));
    }

    #[test]
    fn site_file_listing_can_include_build_dirs_for_session_views() {
        assert!(!should_skip_listing_dir("dist", true));
        assert!(!should_skip_listing_dir("out", true));
        assert!(!should_skip_listing_dir("docs", true));
        assert!(!should_skip_listing_dir("build", true));
        assert!(should_skip_listing_dir("output_old", true));
        assert!(should_skip_listing_dir("node_modules", true));
        assert!(should_skip_listing_dir("target", true));
    }

    #[test]
    fn pagination_defaults() {
        let json = r#"{}"#;
        let params: PaginationParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 100);
        assert_eq!(params.offset, 0);
        assert_eq!(params.since_seq, None);
        assert_eq!(params.topic, None);
    }

    #[test]
    fn pagination_custom_values() {
        let json = r#"{"limit": 50, "offset": 10, "since_seq": 3, "topic": "slides demo"}"#;
        let params: PaginationParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 50);
        assert_eq!(params.offset, 10);
        assert_eq!(params.since_seq, Some(3));
        assert_eq!(params.topic.as_deref(), Some("slides demo"));
    }

    #[test]
    fn internal_api_session_ids_are_hidden_from_session_list() {
        assert!(is_internal_api_session_id("web-123#child-task-1"));
        assert!(is_internal_api_session_id("web-123#default.tasks"));
        assert!(is_internal_api_session_id("web-123#research.tasks"));
        assert!(!is_internal_api_session_id("web-123#research"));
        assert!(!is_internal_api_session_id("web-123"));
    }

    #[tokio::test]
    async fn list_sessions_hides_internal_runtime_sessions_from_standalone_store() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));
        let parent = SessionKey::with_profile(MAIN_PROFILE_ID, "api", "web-123");
        let child = SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", "web-123", "child-1");
        let task_ledger =
            SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", "web-123", "default.tasks");
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
        }
        let state = std::sync::Arc::new(AppState {
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });

        let response = list_sessions(State(state), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let sessions: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = sessions
            .iter()
            .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
            .collect();

        assert_eq!(ids, vec!["web-123"]);
    }

    /// Issue #607 §D regression: river / mini4 hung 30 s+ on
    /// `GET /api/sessions` because one user dir had 65 535
    /// `child-*.jsonl` files that the listing iterated. The handler must
    /// stay well under 500 ms with a synthetic spawn fanout in place.
    #[tokio::test]
    async fn list_sessions_is_fast_with_many_child_jsonl_files() {
        let data_dir = tempfile::tempdir().unwrap();

        // Lay down the per-user dir for `_main:api:web-river` directly so the
        // test does not have to drive 10 000 SessionHandle writes (~30 s on
        // its own and not what we are measuring).
        let encoded_base = "_main%3Aapi%3Aweb-river";
        let user_dir = data_dir
            .path()
            .join("users")
            .join(encoded_base)
            .join("sessions");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(
            user_dir.join("default.jsonl"),
            "{\"schema_version\":1,\"session_key\":\"_main:api:web-river\",\
             \"created_at\":\"2024-01-01T00:00:00Z\",\
             \"updated_at\":\"2024-01-01T00:00:00Z\"}\n",
        )
        .unwrap();

        const FANOUT: usize = 10_000;
        for i in 0..FANOUT {
            std::fs::write(
                user_dir.join(format!("child-task-{i:05}.jsonl")),
                "{\"schema_version\":1}\n{\"role\":\"assistant\",\"content\":\"x\"}\n",
            )
            .unwrap();
        }

        let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));
        let state = std::sync::Arc::new(AppState {
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });

        let start = std::time::Instant::now();
        let response = list_sessions(State(state), HeaderMap::new()).await;
        let elapsed = start.elapsed();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let listed: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = listed
            .iter()
            .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
            .collect();
        assert_eq!(ids, vec!["web-river"], "child fanout must not surface");

        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "list_sessions took {elapsed:?} for {FANOUT} child jsonls; \
             #607 §D regression — the user-facing handler must not iterate child fanouts",
        );
    }

    #[tokio::test]
    async fn delete_session_accepts_listed_topic_session_id_from_standalone_store() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));
        let topic_key =
            SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", "web-topic", "research");
        {
            let mut sess = sessions.lock().await;
            sess.add_message(&topic_key, Message::user("topic"))
                .await
                .unwrap();
            assert!(sess.load(&topic_key).await.is_some());
        }
        let state = std::sync::Arc::new(AppState {
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });

        let response = delete_session(
            State(state),
            HeaderMap::new(),
            axum::extract::Path("web-topic#research".to_string()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let fresh = octos_bus::SessionManager::open(data_dir.path()).unwrap();
        assert!(fresh.load(&topic_key).await.is_none());
    }

    #[test]
    fn session_messages_proxy_path_includes_topic_and_since_seq() {
        let path = session_messages_proxy_path(
            "slides-123",
            100,
            5,
            Some("full"),
            Some(8),
            Some("slides untitled-deck"),
        );

        assert_eq!(
            path,
            "/sessions/slides-123/messages?limit=100&offset=5&source=full&since_seq=8&topic=slides%20untitled-deck"
        );
    }

    #[test]
    fn append_topic_query_uses_question_mark_for_clean_path() {
        let mut path = "/sessions/slides-123/tasks".to_string();
        append_topic_query(&mut path, Some("slides untitled-deck"));
        assert_eq!(
            path,
            "/sessions/slides-123/tasks?topic=slides%20untitled-deck"
        );
    }

    #[test]
    fn append_topic_query_uses_ampersand_when_query_exists() {
        let mut path = "/sessions/slides-123/messages?limit=100".to_string();
        append_topic_query(&mut path, Some("slides untitled-deck"));
        assert_eq!(
            path,
            "/sessions/slides-123/messages?limit=100&topic=slides%20untitled-deck"
        );
    }

    #[test]
    fn append_since_seq_query_uses_question_mark_for_clean_path() {
        let mut path = "/sessions/slides-123/events/stream".to_string();
        append_since_seq_query(&mut path, Some(8));
        assert_eq!(path, "/sessions/slides-123/events/stream?since_seq=8");
    }

    #[test]
    fn append_since_seq_query_uses_ampersand_when_query_exists() {
        let mut path = "/sessions/slides-123/events/stream?topic=slides".to_string();
        append_since_seq_query(&mut path, Some(8));
        assert_eq!(
            path,
            "/sessions/slides-123/events/stream?topic=slides&since_seq=8"
        );
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

    #[tokio::test]
    async fn chat_sync_writes_to_canonical_per_user_topic_jsonl() {
        // Regression for the standalone `octos serve` /chat handlers writing
        // to the legacy flat layout instead of the canonical per-user JSONL.
        //
        // Pre-fix, `chat_sync`/`chat_streaming`/the websocket handler all
        // called `SessionManager::add_message_with_seq` directly. That writes
        // to `<data_dir>/sessions/<encoded_full_key>.jsonl` (legacy flat),
        // not the canonical per-user `<topic>.jsonl`. A standalone deployment
        // without a gateway-side `ApiChannel` therefore split-brained — the
        // actor wrote to one path, handlers wrote to another, replays missed
        // half the history.
        //
        // Post-fix, every `/chat` write must funnel through
        // `persist_chat_message_through_canonical` — a wrapper around
        // `octos_bus::persist_message_through_canonical_path` that also
        // invalidates the `SessionManager` LRU cache (mirroring what
        // `ApiChannel::persist_to_session` does). The contract: messages
        // committed via this helper land in the canonical per-user JSONL
        // and never touch the legacy flat directory.
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));
        let session_id = "web-canonical-handlers";
        let topic = "research";
        let key = SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", session_id, topic);

        // Drive a write through the helper the production handlers now use.
        let seq = persist_chat_message_through_canonical(
            &sessions,
            &key,
            Message::user("please summarise the q1 numbers"),
        )
        .await
        .expect("canonical persist");
        assert_eq!(seq, 0);

        // Canonical per-user `<encoded_topic>.jsonl` must exist and carry
        // the user message.
        let encoded_base = octos_bus::session::encode_path_component(&format!(
            "{MAIN_PROFILE_ID}:api:{session_id}"
        ));
        let encoded_topic = octos_bus::session::encode_path_component(topic);
        let canonical = data_dir
            .path()
            .join("users")
            .join(&encoded_base)
            .join("sessions")
            .join(format!("{encoded_topic}.jsonl"));
        assert!(
            canonical.exists(),
            "/chat handler write must land in canonical per-user JSONL ({}) — \
             this is the unified file the SessionActor and the bus-side \
             ApiChannel also write",
            canonical.display()
        );
        let body = std::fs::read_to_string(&canonical).unwrap();
        assert!(
            body.contains("please summarise the q1 numbers"),
            "canonical JSONL must contain the user message text"
        );

        // Legacy flat `sessions/<encoded_full_key>.jsonl` must NOT exist —
        // that's the old split-brain location standalone /chat used to
        // write to.
        let sessions_dir = data_dir.path().join("sessions");
        if sessions_dir.exists() {
            for entry in std::fs::read_dir(&sessions_dir).unwrap().flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    panic!(
                        "/chat handler must NOT write to the legacy flat \
                         layout (sessions/{}.jsonl) — that is the split-brain \
                         path the storage unification PR is closing",
                        name
                    );
                }
            }
        }
    }

    // ────────── M7.9 / W2 cancel + restart-from-node API tests ──────────

    /// Build a `task_query_store` carrying a single live supervisor with a
    /// running task pre-registered. Returns (store, supervisor, task_id).
    fn build_task_store_with_running_task(
        session_key: &str,
        tool_name: &str,
        tool_call_id: &str,
    ) -> (
        crate::session_actor::SessionTaskQueryStore,
        Arc<octos_agent::TaskSupervisor>,
        String,
    ) {
        let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
        let task_id = supervisor.register(tool_name, tool_call_id, Some(session_key));
        supervisor.mark_running(&task_id);
        let store = crate::session_actor::SessionTaskQueryStore::default();
        let encoded = octos_bus::session::encode_path_component(session_key);
        let key = octos_core::SessionKey::new(MAIN_PROFILE_ID, &encoded);
        let tmp = tempfile::tempdir().unwrap();
        store.register(&key, &supervisor, tmp.path());
        (store, supervisor, task_id)
    }

    #[tokio::test]
    async fn cancel_task_returns_200_when_running_task_is_cancelled() {
        let (store, _supervisor, task_id) =
            build_task_store_with_running_task("api-cancel-1", "run_pipeline", "call-x");
        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });

        let response = cancel_task(
            State(state),
            HeaderMap::new(),
            axum::extract::Path(task_id.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["task_id"], task_id);
        assert_eq!(json["status"], "cancelled");
    }

    #[tokio::test]
    async fn cancel_task_returns_404_for_unknown_task() {
        let store = crate::session_actor::SessionTaskQueryStore::default();
        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let response = cancel_task(
            State(state),
            HeaderMap::new(),
            axum::extract::Path("does-not-exist".to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_task_returns_409_when_already_terminal() {
        let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
        let task_id = supervisor.register("run_pipeline", "call-409", Some("session"));
        supervisor.mark_completed(&task_id, vec![]);

        let store = crate::session_actor::SessionTaskQueryStore::default();
        let encoded = octos_bus::session::encode_path_component("session");
        let key = octos_core::SessionKey::new(MAIN_PROFILE_ID, &encoded);
        let tmp = tempfile::tempdir().unwrap();
        store.register(&key, &supervisor, tmp.path());

        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let response =
            cancel_task(State(state), HeaderMap::new(), axum::extract::Path(task_id)).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn cancel_task_returns_503_without_task_query_store() {
        let state = Arc::new(AppState::empty_for_tests());
        let response = cancel_task(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            axum::extract::Path("any".to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn restart_task_from_node_returns_200_with_new_task_id() {
        let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
        let task_id = supervisor.register("run_pipeline", "call-restart", Some("session"));
        supervisor.mark_running(&task_id);
        supervisor.mark_failed(&task_id, "design phase failed".to_string());

        let store = crate::session_actor::SessionTaskQueryStore::default();
        let encoded = octos_bus::session::encode_path_component("session");
        let key = octos_core::SessionKey::new(MAIN_PROFILE_ID, &encoded);
        let tmp = tempfile::tempdir().unwrap();
        store.register(&key, &supervisor, tmp.path());

        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let response = restart_task_from_node(
            State(state),
            HeaderMap::new(),
            axum::extract::Path(task_id.clone()),
            Some(Json(RestartFromNodeRequest {
                node_id: Some("design".into()),
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["original_task_id"], task_id);
        assert_eq!(json["from_node"], "design");
        assert!(
            json["new_task_id"].as_str().unwrap().len() > 0,
            "new_task_id should be a fresh UUID-ish string"
        );

        // Upstream cached outputs are preserved by virtue of the original
        // task being left intact in the supervisor — the relaunch only
        // adds a successor in the `Spawned` state.
        let original = supervisor.get_task(&task_id).unwrap();
        assert_eq!(original.status, octos_agent::TaskStatus::Failed);
        let new_id = json["new_task_id"].as_str().unwrap();
        let successor = supervisor.get_task(new_id).unwrap();
        assert_eq!(successor.tool_name, "run_pipeline");
    }

    #[tokio::test]
    async fn restart_task_from_node_returns_404_for_unknown_task() {
        let store = crate::session_actor::SessionTaskQueryStore::default();
        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let response = restart_task_from_node(
            State(state),
            HeaderMap::new(),
            axum::extract::Path("nope".into()),
            Some(Json(RestartFromNodeRequest::default())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn restart_task_from_node_returns_409_for_active_task() {
        let (store, _supervisor, task_id) =
            build_task_store_with_running_task("api-restart-409", "run_pipeline", "call-y");
        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let response = restart_task_from_node(
            State(state),
            HeaderMap::new(),
            axum::extract::Path(task_id),
            Some(Json(RestartFromNodeRequest::default())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}
