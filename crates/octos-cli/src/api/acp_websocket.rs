//! ACP WebSocket handler for the serve command.
//!
//! Provides a WebSocket endpoint at /acp that implements the Agent Communication Protocol.

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
};
use futures::{SinkExt, StreamExt};
use octos_agent::progress::{ProgressEvent, ProgressReporter};
use octos_core::SessionKey;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::AppState;

/// JSON-RPC 2.0 Request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JsonRpcRequest {
    pub(crate) jsonrpc: String,
    pub(crate) id: String,
    pub(crate) method: String,
    pub(crate) params: serde_json::Value,
}

/// JSON-RPC 2.0 Response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JsonRpcResponse {
    pub(crate) jsonrpc: String,
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<AcpError>,
}

/// JSON-RPC 2.0 Notification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JsonRpcNotification {
    pub(crate) jsonrpc: String,
    pub(crate) method: String,
    pub(crate) params: serde_json::Value,
}

/// ACP message types (for internal use)
#[derive(Debug, Clone)]
pub(crate) enum AcpMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

impl AcpMessage {
    fn to_json_string(&self) -> Result<String, serde_json::Error> {
        match self {
            AcpMessage::Request(req) => serde_json::to_string(req),
            AcpMessage::Response(resp) => serde_json::to_string(resp),
            AcpMessage::Notification(notif) => serde_json::to_string(notif),
        }
    }
}

/// ACP error structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AcpError {
    pub(crate) code: i32,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<serde_json::Value>,
}

/// Progress reporter that sends ACP session/update notifications via WebSocket
struct WebSocketStreamReporter {
    session_id: String,
    sender: mpsc::UnboundedSender<AcpMessage>,
}

impl WebSocketStreamReporter {
    fn new(session_id: String, sender: mpsc::UnboundedSender<AcpMessage>) -> Self {
        Self { session_id, sender }
    }
}

impl ProgressReporter for WebSocketStreamReporter {
    fn report(&self, event: ProgressEvent) {
        match event {
            ProgressEvent::StreamChunk { text, .. } => {
                let notification = AcpMessage::Notification(JsonRpcNotification {
                    jsonrpc: "2.0".to_string(),
                    method: "session/update".to_string(),
                    params: serde_json::json!({
                        "sessionId": self.session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {
                                "type": "text",
                                "text": text
                            }
                        }
                    }),
                });
                let _ = self.sender.send(notification);
            }
            _ => {}
        }
    }
}

/// Handle WebSocket upgrade request at /acp
pub async fn acp_websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    info!("ACP WebSocket connection request");
    ws.on_upgrade(move |socket| handle_acp_socket(socket, state))
}

/// Handle an established WebSocket connection
async fn handle_acp_socket(socket: WebSocket, state: Arc<AppState>) {
    info!("ACP WebSocket connection established");

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Unique session key for this connection so conversation history is isolated.
    let session_key = SessionKey::new("acp", &Uuid::new_v4().to_string());

    // Unbounded sender keeps report() synchronous (ProgressReporter::report is not async).
    let (tx, mut rx): (mpsc::UnboundedSender<AcpMessage>, mpsc::UnboundedReceiver<AcpMessage>) =
        mpsc::unbounded_channel();

    // Send welcome notification
    let welcome = AcpMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "connected".to_string(),
        params: serde_json::json!({
            "message": "Connected to Octos ACP Bridge",
            "version": "1.0.0"
        }),
    });
    if let Err(e) = tx.send(welcome) {
        error!("Failed to send welcome message: {}", e);
        return;
    }

    // Spawn task to forward messages from channel to WebSocket
    let mut send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Ok(json) = msg.to_json_string() {
                if ws_sender.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
        }
    });

    // Main message loop
    let mut recv_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    debug!("Received ACP message: {}", text);
                    if let Err(e) =
                        handle_acp_message(&text, tx.clone(), &state, &session_key).await
                    {
                        // A channel send error means the send_task has exited; close cleanly.
                        error!("ACP connection lost: {}", e);
                        break;
                    }
                }
                Ok(Message::Binary(data)) => {
                    warn!("Received binary message, ignoring: {} bytes", data.len());
                }
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                    // Axum handles ping/pong automatically
                }
                Ok(Message::Close(frame)) => {
                    info!("WebSocket close frame received: {:?}", frame);
                    break;
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = &mut send_task => {
            info!("Send task completed");
        }
        _ = &mut recv_task => {
            info!("Receive task completed");
        }
    }

    info!("ACP WebSocket connection closed");
}

/// Handle an incoming ACP message
async fn handle_acp_message(
    message: &str,
    sender: mpsc::UnboundedSender<AcpMessage>,
    state: &AppState,
    session_key: &SessionKey,
) -> eyre::Result<()> {
    // Try to parse as JSON-RPC request
    match serde_json::from_str::<JsonRpcRequest>(message) {
        Ok(request) => {
            info!("Handling ACP request: {} ({})", request.method, request.id);

            let response = match request.method.as_str() {
                "chat" | "user_input" => {
                    handle_chat_request(
                        request.id.clone(),
                        request.params,
                        state,
                        sender.clone(),
                        session_key.clone(),
                    )
                    .await
                }
                "ping" => Ok(AcpMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id.clone(),
                    result: Some(serde_json::json!({ "pong": true })),
                    error: None,
                })),
                "status" => handle_status_request(request.id.clone(), state).await,
                "list_sessions" => handle_list_sessions_request(request.id.clone(), state).await,
                _ => Ok(AcpMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id.clone(),
                    result: None,
                    error: Some(AcpError {
                        code: -32601,
                        message: format!("Method not found: {}", request.method),
                        data: None,
                    }),
                })),
            };

            let resp = match response {
                Ok(r) => r,
                Err(e) => AcpMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id,
                    result: None,
                    error: Some(AcpError {
                        code: -32603,
                        message: format!("Internal error: {}", e),
                        data: None,
                    }),
                }),
            };
            sender.send(resp)?;
        }
        Err(_) => {
            // JSON-RPC 2.0 §5 — respond with parse error; use null id (unknown).
            warn!("Received invalid JSON-RPC message");
            let error_resp = AcpMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: "null".to_string(),
                result: None,
                error: Some(AcpError {
                    code: -32700,
                    message: "Parse error".to_string(),
                    data: None,
                }),
            });
            sender.send(error_resp)?;
        }
    }

    Ok(())
}

/// Handle a chat/user_input request
async fn handle_chat_request(
    id: String,
    params: serde_json::Value,
    state: &AppState,
    sender: mpsc::UnboundedSender<AcpMessage>,
    session_key: SessionKey,
) -> eyre::Result<AcpMessage> {
    let input = params
        .get("input")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("Missing 'input' parameter"))?;

    let profile_rt = state
        .profiles
        .values()
        .next()
        .ok_or_else(|| eyre::eyre!("No profiles configured"))?;

    let session_rt = state
        .session_cache
        .get_or_init(profile_rt, session_key.clone(), None)
        .await?;

    let history = {
        let mut sess = session_rt.sessions.lock().await;
        let session = sess.get_or_create(&session_key).await;
        session.get_history(50).to_vec()
    };

    let session_id = Uuid::new_v4().to_string();

    let reporter = Arc::new(WebSocketStreamReporter::new(session_id.clone(), sender.clone()));
    session_rt.agent.set_reporter(reporter);

    let result = session_rt.agent.process_message(input, &history, vec![]).await;

    match result {
        Ok(response) => {
            {
                let mut sess = session_rt.sessions.lock().await;
                for msg in &response.messages {
                    let _ = sess.add_message(&session_key, msg.clone()).await;
                }
            }

            Ok(AcpMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({
                    "stopReason": "end_turn",
                    "usage": {
                        "inputTokens": response.token_usage.input_tokens,
                        "outputTokens": response.token_usage.output_tokens,
                        "totalTokens": response.token_usage.input_tokens + response.token_usage.output_tokens,
                    }
                })),
                error: None,
            }))
        }
        Err(e) => Ok(AcpMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(AcpError {
                code: -32603,
                message: format!("Agent error: {}", e),
                data: None,
            }),
        })),
    }
}

/// Handle a status request
async fn handle_status_request(id: String, state: &AppState) -> eyre::Result<AcpMessage> {
    let agent_available = !state.profiles.is_empty();
    let sessions_available = state.sessions.is_some();

    Ok(AcpMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(serde_json::json!({
            "agent_available": agent_available,
            "sessions_available": sessions_available,
            "uptime_seconds": (chrono::Utc::now() - state.started_at).num_seconds(),
        })),
        error: None,
    }))
}

/// Handle a list_sessions request
async fn handle_list_sessions_request(
    id: String,
    state: &AppState,
) -> eyre::Result<AcpMessage> {
    let sessions = state
        .sessions
        .as_ref()
        .ok_or_else(|| eyre::eyre!("Session manager not configured"))?;

    let session_list = {
        let sess = sessions.lock().await;
        sess.list_sessions()
            .into_iter()
            .map(|(key, count)| serde_json::json!({ "key": key, "message_count": count }))
            .collect::<Vec<_>>()
    };

    Ok(AcpMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(serde_json::json!({
            "sessions": session_list,
        })),
        error: None,
    }))
}
