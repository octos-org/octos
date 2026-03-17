//! API request handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use octos_agent::Agent;
use octos_core::{AgentId, Message, SessionKey};
use serde::{Deserialize, Serialize};

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
}

#[derive(Serialize)]
pub struct ChatResponse {
    pub content: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Maximum message length (1MB).
const MAX_MESSAGE_LEN: usize = 1_048_576;

pub async fn chat(State(state): State<Arc<AppState>>, Json(req): Json<ChatRequest>) -> Response {
    // If a gateway has an API channel running, proxy the request to it.
    // This gives the web client access to adaptive routing, queue modes,
    // multi-provider failover, and all gateway features.
    if let Some(pm) = &state.process_manager {
        if let Some((_profile_id, port)) = pm.first_api_port().await {
            return super::webhook_proxy::api_chat_proxy(
                &state,
                port,
                &req.message,
                req.session_id.as_deref(),
            )
            .await;
        }
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
        let session = sess.get_or_create(&session_key);
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
        let session = sess.get_or_create(&session_key);
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

    // Spawn the agent task
    tokio::spawn(async move {
        let result = request_agent
            .process_message(&message, &history, vec![])
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

                // Send final done event
                let done = serde_json::json!({
                    "type": "done",
                    "content": response.content,
                    "input_tokens": response.token_usage.input_tokens,
                    "output_tokens": response.token_usage.output_tokens,
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
#[derive(Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub message_count: usize,
}

pub async fn list_sessions(State(state): State<Arc<AppState>>) -> Response {
    // If local sessions available, use them directly
    if let Some(sessions) = &state.sessions {
        let sess = sessions.lock().await;
        let list: Vec<SessionInfo> = sess
            .list_sessions()
            .into_iter()
            .filter_map(|(id, count)| {
                let chat_id = id.strip_prefix("api:")?;
                Some(SessionInfo {
                    id: chat_id.to_string(),
                    message_count: count,
                })
            })
            .collect();
        return Json(list).into_response();
    }

    // Proxy to gateway if available
    if let Some(pm) = &state.process_manager {
        if let Some((_profile_id, port)) = pm.first_api_port().await {
            return super::webhook_proxy::api_get_proxy(&state, port, "/sessions").await;
        }
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Sessions not available".to_string(),
    )
        .into_response()
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
}

fn default_page_limit() -> usize {
    100
}

pub async fn session_messages(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<PaginationParams>,
) -> Response {
    if let Some(sessions) = &state.sessions {
        let limit = params.limit.min(500);
        let offset = params.offset.min(10_000);
        let fetch_count = match offset.checked_add(limit) {
            Some(n) => n,
            None => return (StatusCode::BAD_REQUEST, "invalid pagination").into_response(),
        };
        let key = SessionKey::new("api", &id);
        let mut sess = sessions.lock().await;
        let session = sess.get_or_create(&key);
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
        return Json(messages).into_response();
    }

    // Proxy to gateway
    if let Some(pm) = &state.process_manager {
        if let Some((_profile_id, port)) = pm.first_api_port().await {
            let path = format!("/sessions/{id}/messages?limit={}&offset={}", params.limit, params.offset);
            return super::webhook_proxy::api_get_proxy(&state, port, &path).await;
        }
    }

    (StatusCode::SERVICE_UNAVAILABLE, "Sessions not available").into_response()
}

#[derive(Serialize)]
pub struct MessageInfo {
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

/// DELETE /api/sessions/:id -- delete a session.
pub async fn delete_session(
    State(state): State<Arc<AppState>>,
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
    if let Some(pm) = &state.process_manager {
        if let Some((_profile_id, port)) = pm.first_api_port().await {
            let path = format!("/sessions/{id}");
            return super::webhook_proxy::api_delete_proxy(&state, port, &path).await;
        }
    }

    (StatusCode::SERVICE_UNAVAILABLE, "Sessions not available").into_response()
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
}
