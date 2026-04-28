//! M7.2 — MCP server mode for `octos mcp-serve`.
//!
//! Exposes octos sessions as MCP tools so outer orchestrators (another
//! octos instance, Codex, Claude Code, hermes) can invoke octos as a
//! sub-agent. This is the mirror of M7.1 (MCP client mode in [`mcp`]).
//!
//! # Tool shape
//!
//! Exactly **one** MCP tool is advertised: `run_octos_session`. Each
//! call runs a full octos session (contract + input → workspace contract
//! artifact) and returns the aggregate result to the caller. Internal
//! tool calls, iteration events, and progress are **never** streamed to
//! the outer MCP caller — the outer caller sees one request/response.
//!
//! # Transports
//!
//! * **stdio** — parent-trust auth. The parent process spawned us, so
//!   no token is required.
//! * **http** — bearer token required (via
//!   `OCTOS_MCP_SERVER_TOKEN`). Missing or wrong → synchronous 401.
//!
//! # Invariants
//!
//! 1. Session-level exposure only; `tools/list` returns `run_octos_session`.
//! 2. Run-to-completion semantics: caller waits for `Ready`/`Failed`.
//! 3. `TaskLifecycleState` transitions propagate to the MCP result via the
//!    `final_state` field.
//! 4. Workspace-contract enforcement runs identically to local dispatch.
//! 5. Every call emits [`HarnessEventPayload::McpServerCall`](crate::harness_events::HarnessEventPayload::McpServerCall)
//!    and increments the `octos_mcp_server_call_total{tool,outcome}` counter.
//! 6. Zero new `unsafe`.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use metrics::counter;
use octos_core::{TASK_RESULT_SCHEMA_VERSION, TaskId};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::harness_events::HarnessEvent;
use crate::task_supervisor::{TaskLifecycleState, TaskSupervisor};

/// MCP protocol version negotiated by `octos mcp-serve`. Stays in sync with
/// the client implementation in [`crate::mcp`].
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// The single session-level MCP tool exposed by the server.
pub const RUN_OCTOS_SESSION_TOOL: &str = "run_octos_session";

/// Environment variable name that the HTTP transport reads for its bearer token.
pub const OCTOS_MCP_SERVER_TOKEN_ENV: &str = "OCTOS_MCP_SERVER_TOKEN";

/// Maximum JSON-RPC request body size (1 MB), matching the MCP client.
const MAX_REQUEST_BYTES: usize = 1_048_576;

/// Typed error kinds surfaced by the session-level dispatch flow.
#[derive(Debug, Clone)]
pub enum McpServerError {
    ProtocolError(String),
    UnknownTool(String),
    InvalidParams(String),
    SessionFailed(String),
    Unauthorized,
}

impl std::fmt::Display for McpServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProtocolError(msg) => write!(f, "protocol error: {msg}"),
            Self::UnknownTool(name) => write!(f, "unknown tool: {name}"),
            Self::InvalidParams(msg) => write!(f, "invalid params: {msg}"),
            Self::SessionFailed(msg) => write!(f, "session failed: {msg}"),
            Self::Unauthorized => f.write_str("authentication required"),
        }
    }
}

impl std::error::Error for McpServerError {}

/// Final coarse outcome of a session run, together with the fields that
/// matter to the outer caller.
#[derive(Debug, Clone)]
pub struct McpSessionOutcome {
    pub final_state: TaskLifecycleState,
    pub artifact_path: Option<String>,
    pub artifact_content: Option<String>,
    pub validator_results: Vec<Value>,
    pub cost: Value,
    pub error: Option<String>,
}

/// Observer used by the session dispatch to record lifecycle transitions
/// (Queued → Running → Verifying → Ready/Failed) without leaking the
/// underlying runtime to the dispatch trait.
pub trait SessionLifecycleObserver: Send + Sync {
    fn mark_state(&self, state: TaskLifecycleState);
}

/// Trait that runs a single octos session given an opaque contract name and
/// an input payload, returning the aggregate outcome. This indirection keeps
/// `mcp_server` testable without pulling the entire chat/gateway bring-up
/// into the acceptance tests.
#[async_trait]
pub trait McpSessionDispatch: Send + Sync + 'static {
    async fn run_session(
        &self,
        contract: &str,
        input: &Value,
        observer: &dyn SessionLifecycleObserver,
    ) -> Result<McpSessionOutcome, McpServerError>;
}

/// Event sink callback — receives each typed `HarnessEvent` emitted by the
/// server. Used both by tests (to assert events landed) and by runtime
/// callers that want to flush MCP audit events into a long-lived sink.
type EventSink = Arc<dyn Fn(HarnessEvent) + Send + Sync>;

/// The session-level MCP server. Cloneable via `Arc` — all shared state is
/// interior-mutable.
pub struct McpServer {
    dispatch: Arc<dyn McpSessionDispatch>,
    supervisor: Arc<TaskSupervisor>,
    event_sink: RwLock<Option<EventSink>>,
}

impl McpServer {
    pub fn new(dispatch: Arc<dyn McpSessionDispatch>, supervisor: Arc<TaskSupervisor>) -> Self {
        Self {
            dispatch,
            supervisor,
            event_sink: RwLock::new(None),
        }
    }

    /// Install an event sink. Events are typed `HarnessEvent` instances; the
    /// sink MAY spawn tasks or enqueue them — the callback is invoked
    /// synchronously by the server.
    pub async fn set_event_sink<F>(&self, f: F)
    where
        F: Fn(HarnessEvent) + Send + Sync + 'static,
    {
        let sink: EventSink = Arc::new(f);
        *self.event_sink.write().await = Some(sink);
    }

    /// Handle a single JSON-RPC request and return the JSON-RPC response.
    ///
    /// `transport` is the label reported in emitted `McpServerCall` events
    /// (`stdio` or `http`).
    pub async fn handle_request(&self, request: &Value, transport: &str) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");

        match method {
            "initialize" => json_rpc_result(id, build_initialize_response(self)),
            "tools/list" => json_rpc_result(id, build_tools_list_response(self)),
            "tools/call" => {
                let empty = Value::Object(Default::default());
                let params = request.get("params").unwrap_or(&empty);
                self.handle_tools_call(id, params, transport).await
            }
            "notifications/initialized" | "ping" => {
                // MCP notifications don't require a reply.
                json!({"jsonrpc": "2.0", "id": id, "result": {}})
            }
            other => render_mcp_error(
                id,
                McpServerError::ProtocolError(format!("unknown method '{other}'")),
            ),
        }
    }

    async fn handle_tools_call(&self, id: Value, params: &Value, transport: &str) -> Value {
        let tool_name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if tool_name != RUN_OCTOS_SESSION_TOOL {
            return render_mcp_error(id, McpServerError::UnknownTool(tool_name.to_string()));
        }

        match dispatch_run_octos_session(&*self.dispatch, &self.supervisor, params).await {
            Ok(result) => {
                // Extract the outcome so we can emit the matching harness event.
                let (outcome_label, contract_label, error_message) =
                    extract_outcome_from_result(&result);
                self.emit_mcp_event(
                    transport,
                    &outcome_label,
                    contract_label.as_deref(),
                    error_message.as_deref(),
                );
                counter!(
                    "octos_mcp_server_call_total",
                    "tool" => RUN_OCTOS_SESSION_TOOL.to_string(),
                    "outcome" => outcome_label,
                )
                .increment(1);
                json_rpc_result(id, result)
            }
            Err(err) => {
                self.emit_mcp_event(transport, "error", None, Some(&err.to_string()));
                counter!(
                    "octos_mcp_server_call_total",
                    "tool" => RUN_OCTOS_SESSION_TOOL.to_string(),
                    "outcome" => "error".to_string(),
                )
                .increment(1);
                render_mcp_error(id, err)
            }
        }
    }

    fn emit_mcp_event(
        &self,
        transport: &str,
        outcome: &str,
        contract: Option<&str>,
        error: Option<&str>,
    ) {
        let Some(sink) = self.event_sink.try_read().ok().and_then(|g| g.clone()) else {
            return;
        };
        let caller_id = caller_id_for_transport(transport);
        let event = HarnessEvent::mcp_server_call(
            format!("mcp:{transport}"),
            TaskId::new().to_string(),
            RUN_OCTOS_SESSION_TOOL,
            caller_id,
            transport,
            outcome,
            contract.map(|s| s.to_string()),
            error.map(|s| s.to_string()),
        );
        (sink)(event);
    }

    /// Bind an HTTP listener on localhost (ephemeral port) for tests.
    /// Production callers should use [`McpServer::serve_http`] with an
    /// explicit [`SocketAddr`].
    pub async fn spawn_http_on_local_port(self, token: String) -> std::io::Result<McpServerHandle> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        self.serve_http_on_listener(listener, token).await
    }

    /// Bind an HTTP listener on `addr` and spawn it as a background task,
    /// returning a handle that observes the bound address and allows the
    /// caller to shut the listener down. Production entry point.
    pub async fn serve_http(
        self,
        addr: SocketAddr,
        token: String,
    ) -> std::io::Result<McpServerHandle> {
        let listener = TcpListener::bind(addr).await?;
        self.serve_http_on_listener(listener, token).await
    }

    async fn serve_http_on_listener(
        self,
        listener: TcpListener,
        token: String,
    ) -> std::io::Result<McpServerHandle> {
        let addr = listener.local_addr()?;
        let server = Arc::new(self);
        let token = Arc::new(token);
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let accept_shutdown = shutdown.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_shutdown.notified() => break,
                    accept = listener.accept() => {
                        match accept {
                            Ok((stream, peer)) => {
                                let server = server.clone();
                                let token = token.clone();
                                tokio::spawn(async move {
                                    if let Err(err) = handle_http_connection(server, token, stream, peer).await {
                                        warn!(error = %err, "mcp-serve http connection error");
                                    }
                                });
                            }
                            Err(err) => {
                                warn!(error = %err, "mcp-serve accept failed");
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(McpServerHandle {
            addr,
            shutdown,
            handle,
        })
    }

    /// Production entry point for the stdio transport — loops on stdin, writes
    /// JSON-RPC responses to stdout. Returns when stdin closes or the process
    /// is signalled to shut down.
    pub async fn serve_stdio(self) -> Result<()> {
        let server = Arc::new(self);
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        serve_stdio_generic(server, stdin, stdout).await
    }
}

/// Handle returned by [`McpServer::spawn_http_on_local_port`] so callers can
/// observe the bound address and shut the listener down.
pub struct McpServerHandle {
    addr: SocketAddr,
    shutdown: Arc<tokio::sync::Notify>,
    handle: JoinHandle<()>,
}

impl McpServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub async fn shutdown(self) {
        self.shutdown.notify_waiters();
        let _ = self.handle.await;
    }
}

/// Build the `initialize` response. Public so transports outside this module
/// (notably the CLI integration layer) can reuse it.
pub fn build_initialize_response(_server: &McpServer) -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": {"listChanged": false},
        },
        "serverInfo": {
            "name": "octos",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// Build the `tools/list` response advertising the single session-level tool.
pub fn build_tools_list_response(_server: &McpServer) -> Value {
    json!({
        "tools": [{
            "name": RUN_OCTOS_SESSION_TOOL,
            "description": "Run a complete octos session. The caller supplies a workspace contract name and an input payload; octos runs its normal loop to completion (including workspace-contract enforcement) and returns the resulting artifact. Internal tool calls and progress events are not streamed to the caller.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "contract": {
                        "type": "string",
                        "description": "Workspace contract name (e.g. 'slides_delivery', 'site_delivery', 'coding')."
                    },
                    "input": {
                        "type": "object",
                        "description": "Opaque input payload forwarded to the session. Shape is contract-specific."
                    }
                },
                "required": ["contract", "input"]
            }
        }]
    })
}

/// Render an [`McpServerError`] into a JSON-RPC error envelope.
pub fn render_mcp_error(id: Value, error: McpServerError) -> Value {
    let (code, message) = match &error {
        McpServerError::ProtocolError(msg) => (-32600, msg.clone()),
        McpServerError::UnknownTool(name) => (-32601, format!("unknown tool: {name}")),
        McpServerError::InvalidParams(msg) => (-32602, msg.clone()),
        McpServerError::SessionFailed(msg) => (-32000, msg.clone()),
        McpServerError::Unauthorized => (-32001, "authentication required".into()),
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

/// Parse `Authorization: Bearer <token>`, returning the raw token.
///
/// Accepts mixed case `Bearer` (case-insensitive) per RFC 6750 §2.1. Returns
/// `None` for any other scheme or a missing header.
pub fn parse_bearer_token(header: Option<&str>) -> Option<String> {
    let raw = header?.trim();
    let (scheme, rest) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

/// Dispatch a `run_octos_session` call by forwarding to the trait, then
/// format the outcome into the standard MCP `tools/call` result.
pub async fn dispatch_run_octos_session(
    dispatch: &dyn McpSessionDispatch,
    supervisor: &TaskSupervisor,
    params: &Value,
) -> Result<Value, McpServerError> {
    let empty = Value::Object(Default::default());
    let arguments = params.get("arguments").unwrap_or(&empty);
    let contract = arguments
        .get("contract")
        .and_then(Value::as_str)
        .ok_or_else(|| McpServerError::InvalidParams("missing 'contract' field".into()))?
        .to_string();
    let input = arguments
        .get("input")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    let task_id = supervisor.register(RUN_OCTOS_SESSION_TOOL, "mcp-call", Some("mcp:server"));
    let observer = SupervisorObserver {
        supervisor,
        task_id: task_id.clone(),
    };
    let result = dispatch.run_session(&contract, &input, &observer).await;
    // Always clear active state from the supervisor; mark failed/completed so
    // restarts replay an accurate snapshot.
    let outcome = match &result {
        Ok(o) => o.clone(),
        Err(err) => {
            supervisor.mark_failed(&task_id, err.to_string());
            return Err(err.clone());
        }
    };
    match outcome.final_state {
        TaskLifecycleState::Ready => {
            let files = outcome
                .artifact_path
                .iter()
                .cloned()
                .collect::<Vec<String>>();
            supervisor.mark_completed(&task_id, files);
        }
        TaskLifecycleState::Failed => {
            supervisor.mark_failed(
                &task_id,
                outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| "session failed".into()),
            );
        }
        _ => {
            // Non-terminal intermediate state: leave the supervisor in its
            // current snapshot (run_session marked it already via observer).
        }
    }
    Ok(build_run_session_result(&contract, &outcome))
}

fn build_run_session_result(contract: &str, outcome: &McpSessionOutcome) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("schema_version".into(), json!(TASK_RESULT_SCHEMA_VERSION));
    body.insert(
        "final_state".into(),
        Value::String(lifecycle_label(outcome.final_state).into()),
    );
    body.insert("contract".into(), Value::String(contract.to_string()));
    if let Some(path) = &outcome.artifact_path {
        body.insert("artifact_path".into(), Value::String(path.clone()));
    }
    if let Some(content) = &outcome.artifact_content {
        body.insert("artifact_content".into(), Value::String(content.clone()));
    }
    body.insert(
        "validator_results".into(),
        Value::Array(outcome.validator_results.clone()),
    );
    body.insert("cost".into(), outcome.cost.clone());
    if let Some(error) = &outcome.error {
        body.insert("error".into(), Value::String(error.clone()));
    }

    let is_error = outcome.final_state == TaskLifecycleState::Failed;
    let text = serde_json::to_string(&Value::Object(body))
        .unwrap_or_else(|_| "{\"error\":\"serialize failed\"}".into());
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error,
    })
}

fn lifecycle_label(state: TaskLifecycleState) -> &'static str {
    match state {
        TaskLifecycleState::Queued => "queued",
        TaskLifecycleState::Running => "running",
        TaskLifecycleState::Verifying => "verifying",
        TaskLifecycleState::Ready => "ready",
        TaskLifecycleState::Failed => "failed",
        TaskLifecycleState::Cancelled => "cancelled",
    }
}

fn extract_outcome_from_result(result: &Value) -> (String, Option<String>, Option<String>) {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let outcome = parsed
        .get("final_state")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let contract = parsed
        .get("contract")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let error = parsed
        .get("error")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    (outcome, contract, error)
}

fn caller_id_for_transport(transport: &str) -> String {
    match transport {
        "stdio" => {
            std::env::var("OCTOS_MCP_CALLER_LABEL").unwrap_or_else(|_| "parent-process".into())
        }
        "http" => "http-bearer".into(),
        other => format!("unknown:{other}"),
    }
}

/// Fingerprint a token (SHA-256, hex, truncated to 12 chars) for event logs.
/// The raw token NEVER appears in events or metrics.
pub fn fingerprint_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

fn json_rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

struct SupervisorObserver<'a> {
    supervisor: &'a TaskSupervisor,
    task_id: String,
}

impl SessionLifecycleObserver for SupervisorObserver<'_> {
    fn mark_state(&self, state: TaskLifecycleState) {
        use crate::task_supervisor::TaskRuntimeState;
        match state {
            TaskLifecycleState::Queued => {
                // already queued at register()
            }
            TaskLifecycleState::Running => self.supervisor.mark_running(&self.task_id),
            TaskLifecycleState::Verifying => {
                self.supervisor.mark_runtime_state(
                    &self.task_id,
                    TaskRuntimeState::VerifyingOutputs,
                    Some("mcp-serve verify".into()),
                );
            }
            TaskLifecycleState::Ready => {
                // Completed state is finalized by dispatch_run_octos_session
                // with the output_files list. Do nothing here to avoid
                // racing with the authoritative completion write.
            }
            TaskLifecycleState::Failed => {
                // Same reasoning as Ready — finalization happens outside.
            }
            TaskLifecycleState::Cancelled => {
                // Cancellation is driven by the supervisor's `cancel`
                // primitive; the observer just acknowledges that the
                // outer caller already moved the task into Cancelled.
            }
        }
    }
}

// ---- stdio transport ----

async fn serve_stdio_generic<R, W>(server: Arc<McpServer>, stdin: R, mut stdout: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            info!("mcp-serve stdio: peer closed");
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > MAX_REQUEST_BYTES {
            let err = render_mcp_error(
                Value::Null,
                McpServerError::ProtocolError("request exceeds 1MB limit".into()),
            );
            let mut buf = serde_json::to_string(&err)?;
            buf.push('\n');
            stdout.write_all(buf.as_bytes()).await?;
            stdout.flush().await?;
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(err) => {
                let response = render_mcp_error(
                    Value::Null,
                    McpServerError::ProtocolError(format!("invalid JSON-RPC: {err}")),
                );
                let mut buf = serde_json::to_string(&response)?;
                buf.push('\n');
                stdout.write_all(buf.as_bytes()).await?;
                stdout.flush().await?;
                continue;
            }
        };

        let response = server.handle_request(&request, "stdio").await;
        let mut buf = serde_json::to_string(&response)?;
        buf.push('\n');
        stdout.write_all(buf.as_bytes()).await?;
        stdout.flush().await?;
    }
}

// ---- http transport (minimal HTTP/1.1, one request per connection) ----

async fn handle_http_connection(
    server: Arc<McpServer>,
    token: Arc<String>,
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
) -> Result<()> {
    let (read, mut write) = stream.split();
    let mut reader = BufReader::new(read);
    let mut request_line = String::new();
    let bytes = reader.read_line(&mut request_line).await?;
    if bytes == 0 {
        return Ok(());
    }
    let request_line = request_line.trim_end_matches(['\r', '\n']).to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    if method != "POST" {
        return write_http_response(
            &mut write,
            405,
            "Method Not Allowed",
            "application/json",
            br#"{"error":"method not allowed"}"#,
        )
        .await;
    }
    // We only accept requests on /, /mcp, or /mcp/jsonrpc to leave routing room
    // but keep the surface tight.
    if !matches!(path, "/" | "/mcp" | "/mcp/jsonrpc") {
        return write_http_response(
            &mut write,
            404,
            "Not Found",
            "application/json",
            br#"{"error":"not found"}"#,
        )
        .await;
    }

    let mut content_length: Option<usize> = None;
    let mut authorization: Option<String> = None;
    loop {
        let mut header_line = String::new();
        let n = reader.read_line(&mut header_line).await?;
        if n == 0 {
            break;
        }
        let trimmed = header_line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().ok();
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.to_string());
            }
        }
    }

    let supplied = parse_bearer_token(authorization.as_deref());
    if !supplied
        .as_deref()
        .is_some_and(|t| constant_time_eq(t, &token))
    {
        return write_http_response(
            &mut write,
            401,
            "Unauthorized",
            "application/json",
            br#"{"error":"unauthorized"}"#,
        )
        .await;
    }

    let length = content_length.unwrap_or(0);
    if length > MAX_REQUEST_BYTES {
        return write_http_response(
            &mut write,
            413,
            "Payload Too Large",
            "application/json",
            br#"{"error":"request body too large"}"#,
        )
        .await;
    }
    let mut body = vec![0_u8; length];
    if length > 0 {
        reader.read_exact(&mut body).await?;
    }

    let request: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            let error_response = render_mcp_error(
                Value::Null,
                McpServerError::ProtocolError(format!("invalid JSON: {err}")),
            );
            let body = serde_json::to_vec(&error_response)?;
            return write_http_response(&mut write, 400, "Bad Request", "application/json", &body)
                .await;
        }
    };

    let response = server.handle_request(&request, "http").await;
    let body = serde_json::to_vec(&response)?;
    write_http_response(&mut write, 200, "OK", "application/json", &body).await?;
    info!(peer = %peer, "mcp-serve http: served request");
    Ok(())
}

async fn write_http_response<W>(
    stream: &mut W,
    status_code: u16,
    status_text: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let header = format!(
        "HTTP/1.1 {status_code} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        len = body.len(),
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

/// Constant-time comparison of two bytes buffers, used to avoid timing leaks
/// on the bearer-token check.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---- simple pass-through observer wrapper for the Arc<Mutex<_>> flavour ----

/// Lock-based observer that records lifecycle transitions into a shared
/// vector. Exposed for integration tests.
pub struct RecordingObserver {
    states: Mutex<Vec<TaskLifecycleState>>,
}

impl RecordingObserver {
    pub fn new() -> Self {
        Self {
            states: Mutex::new(Vec::new()),
        }
    }

    pub async fn states(&self) -> Vec<TaskLifecycleState> {
        self.states.lock().await.clone()
    }
}

impl Default for RecordingObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionLifecycleObserver for RecordingObserver {
    fn mark_state(&self, state: TaskLifecycleState) {
        // Lock is always local; blocking for microseconds is acceptable.
        if let Ok(mut guard) = self.states.try_lock() {
            guard.push(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bearer_token_accepts_mixed_case_scheme() {
        assert_eq!(
            parse_bearer_token(Some("Bearer tok")).as_deref(),
            Some("tok")
        );
        assert_eq!(
            parse_bearer_token(Some("bearer tok")).as_deref(),
            Some("tok")
        );
        assert_eq!(
            parse_bearer_token(Some("BEARER tok")).as_deref(),
            Some("tok")
        );
    }

    #[test]
    fn parse_bearer_token_rejects_other_schemes() {
        assert_eq!(parse_bearer_token(Some("Basic tok")), None);
        assert_eq!(parse_bearer_token(None), None);
        assert_eq!(parse_bearer_token(Some("")), None);
        assert_eq!(parse_bearer_token(Some("Bearer  ")), None);
    }

    #[test]
    fn constant_time_eq_is_length_sensitive() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("abc", "abd"));
    }

    #[test]
    fn fingerprint_token_is_deterministic_and_not_raw() {
        let fp = fingerprint_token("super-secret");
        assert!(fp.starts_with("sha256:"));
        assert!(!fp.contains("super-secret"));
        assert_eq!(fp, fingerprint_token("super-secret"));
    }

    #[test]
    fn render_mcp_error_maps_codes_for_each_variant() {
        for (err, code) in [
            (McpServerError::ProtocolError("p".into()), -32600),
            (McpServerError::UnknownTool("t".into()), -32601),
            (McpServerError::InvalidParams("i".into()), -32602),
            (McpServerError::SessionFailed("s".into()), -32000),
            (McpServerError::Unauthorized, -32001),
        ] {
            let rendered = render_mcp_error(json!(1), err);
            assert_eq!(rendered["error"]["code"], code);
        }
    }

    #[test]
    fn build_run_session_result_encodes_failure_flag() {
        let outcome = McpSessionOutcome {
            final_state: TaskLifecycleState::Failed,
            artifact_path: None,
            artifact_content: None,
            validator_results: Vec::new(),
            cost: json!({}),
            error: Some("boom".into()),
        };
        let result = build_run_session_result("c", &outcome);
        assert_eq!(result["isError"], true);
        let body: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["final_state"], "failed");
        assert_eq!(body["error"], "boom");
        assert_eq!(body["schema_version"], TASK_RESULT_SCHEMA_VERSION);
    }

    #[test]
    fn build_run_session_result_includes_artifact_on_ready() {
        let outcome = McpSessionOutcome {
            final_state: TaskLifecycleState::Ready,
            artifact_path: Some("out/deck.pptx".into()),
            artifact_content: Some("binary".into()),
            validator_results: vec![json!({"passed": true})],
            cost: json!({"input_tokens": 10}),
            error: None,
        };
        let result = build_run_session_result("slides_delivery", &outcome);
        assert_eq!(result["isError"], false);
        let body: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["artifact_path"], "out/deck.pptx");
        assert_eq!(body["final_state"], "ready");
        assert_eq!(body["contract"], "slides_delivery");
    }

    #[test]
    fn lifecycle_label_covers_every_state() {
        for state in [
            TaskLifecycleState::Queued,
            TaskLifecycleState::Running,
            TaskLifecycleState::Verifying,
            TaskLifecycleState::Ready,
            TaskLifecycleState::Failed,
        ] {
            assert!(!lifecycle_label(state).is_empty());
        }
    }

    #[test]
    fn extract_outcome_returns_unknown_for_malformed_result() {
        let result = json!({"content":[{"type":"text","text":"not-json"}],"isError": false});
        let (outcome, contract, error) = extract_outcome_from_result(&result);
        assert_eq!(outcome, "unknown");
        assert!(contract.is_none());
        assert!(error.is_none());
    }
}
