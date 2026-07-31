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

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use metrics::counter;
use octos_core::{TASK_RESULT_SCHEMA_VERSION, TaskId, TokenUsage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler, serve_server};
use tokio_util::sync::CancellationToken;

use crate::harness_events::HarnessEvent;
use crate::task_supervisor::{TaskLifecycleState, TaskSupervisor};
use crate::validators::ValidatorOutcome;

/// MCP protocol version negotiated by `octos mcp-serve`. Stays in sync with
/// the client implementation in [`crate::mcp`].
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// The single session-level MCP tool exposed by the server.
pub const RUN_OCTOS_SESSION_TOOL: &str = "run_octos_session";

/// Environment variable name that the HTTP transport reads for its bearer token.
pub const OCTOS_MCP_SERVER_TOKEN_ENV: &str = "OCTOS_MCP_SERVER_TOKEN";

/// Idle keep-alive for HTTP Streamable sessions. rmcp's 300s default reaps a
/// session mid-call for a long synchronous `run_octos_session` (which emits no
/// intermediate protocol traffic to reset the timer); this widens it to a bound
/// comfortably above a realistic session (the dispatch caps at 20 agent
/// iterations) while still finite, so a session a client abandons without a
/// `DELETE` is reaped instead of leaked. Deployments needing longer single
/// calls can raise this.
const HTTP_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

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
    pub validator_results: Vec<ValidatorOutcome>,
    pub cost: McpSessionCost,
    pub error: Option<String>,
}

/// Stable token-cost projection returned by `run_octos_session`.
///
/// Unlike the internal token accounting type, every counter remains present
/// on the wire, including zero values, to preserve the existing MCP contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSessionCost {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub reasoning_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

impl From<&TokenUsage> for McpSessionCost {
    fn from(usage: &TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        }
    }
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
    // `Arc` so the rmcp `OctosMcpHandler` (which must be `Clone` for the
    // per-session HTTP service factory) can share the same installed sink.
    event_sink: Arc<RwLock<Option<EventSink>>>,
}

impl McpServer {
    pub fn new(dispatch: Arc<dyn McpSessionDispatch>, supervisor: Arc<TaskSupervisor>) -> Self {
        Self {
            dispatch,
            supervisor,
            event_sink: Arc::new(RwLock::new(None)),
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

        let result = dispatch_run_octos_session(&*self.dispatch, &self.supervisor, params).await;
        let sink = self.event_sink.read().await.clone();
        emit_call_outcome(&sink, transport, &result);
        match result {
            Ok(value) => json_rpc_result(id, value),
            Err(err) => render_mcp_error(id, err),
        }
    }

    /// Build the rmcp Streamable HTTP tower service for this server.
    ///
    /// The service factory clones a fresh [`OctosMcpHandler`] (tagged `http`)
    /// per MCP session. When `allow_non_loopback` is `false`, rmcp's default
    /// config restricts requests to loopback `Host` values — a built-in
    /// DNS-rebinding guard appropriate for a `127.0.0.1` bind. Pass `true` when
    /// binding a non-loopback interface for cross-host orchestration; the guard
    /// is then disabled and the bearer token is the sole authenticator.
    ///
    /// The caller (the CLI, which owns axum) mounts this into a router behind a
    /// bearer-token layer and binds it — bearer authentication and TLS are the
    /// caller's responsibility, not this service's.
    ///
    /// Returns the service together with a [`CancellationToken`]: cancelling it
    /// terminates all live sessions so an `axum` graceful shutdown can drain the
    /// long-lived SSE connections instead of hanging until they idle out. The
    /// caller must cancel it in its shutdown path.
    ///
    /// The session manager's idle keep-alive is widened from rmcp's 300s default
    /// to [`HTTP_SESSION_IDLE_TIMEOUT`]: `run_octos_session` is a single
    /// long-running synchronous tool call that emits no intermediate MCP
    /// traffic, so the 300s timer would reap the session mid-call and drop the
    /// result before the client received it. The timer stays finite (not
    /// disabled) so a session a client abandoned without a `DELETE` is still
    /// reaped rather than leaked; the returned token additionally tears every
    /// session down on shutdown.
    pub fn streamable_http_service(
        self,
        allow_non_loopback: bool,
    ) -> (
        StreamableHttpService<OctosMcpHandler, LocalSessionManager>,
        CancellationToken,
    ) {
        let dispatch = self.dispatch;
        let supervisor = self.supervisor;
        let event_sink = self.event_sink;

        let cancel = CancellationToken::new();
        let mut config =
            StreamableHttpServerConfig::default().with_cancellation_token(cancel.clone());
        if allow_non_loopback {
            config = config.disable_allowed_hosts();
        }

        // A long tool call is not a dead session, but a fully-disabled timer
        // leaks abandoned sessions — so widen it to a finite bound instead of
        // clearing it (see the doc comment).
        let mut session_manager = LocalSessionManager::default();
        session_manager.session_config.keep_alive = Some(HTTP_SESSION_IDLE_TIMEOUT);

        let service = StreamableHttpService::new(
            move || {
                Ok(OctosMcpHandler {
                    dispatch: dispatch.clone(),
                    supervisor: supervisor.clone(),
                    event_sink: event_sink.clone(),
                    transport_label: "http",
                })
            },
            Arc::new(session_manager),
            config,
        );
        (service, cancel)
    }

    /// Serve the single `run_octos_session` tool over the rmcp SDK on an
    /// arbitrary byte transport (`read`/`write` pair). [`serve_stdio`] is the
    /// production wrapper over the process's stdin/stdout; tests drive it over
    /// an in-memory duplex. Returns when the peer closes the stream or the
    /// service is cancelled.
    pub async fn serve_io<R, W>(self, read: R, write: W) -> Result<()>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let handler = OctosMcpHandler {
            dispatch: self.dispatch,
            supervisor: self.supervisor,
            event_sink: self.event_sink,
            transport_label: "stdio",
        };
        let running = serve_server(handler, (read, write))
            .await
            .map_err(|err| eyre::eyre!("mcp-serve stdio handshake failed: {err}"))?;
        running
            .waiting()
            .await
            .map_err(|err| eyre::eyre!("mcp-serve stdio service failed: {err}"))?;
        Ok(())
    }

    /// Production entry point for the stdio transport (parent-trust auth — the
    /// parent process spawned us). Serves the single `run_octos_session` tool
    /// over the rmcp SDK on the process's stdin/stdout.
    pub async fn serve_stdio(self) -> Result<()> {
        let (stdin, stdout) = rmcp::transport::stdio();
        self.serve_io(stdin, stdout).await
    }
}

/// rmcp [`ServerHandler`] exposing octos as a single-tool MCP server.
///
/// Every transport (`serve_stdio` today, the streamable-HTTP service next)
/// routes through this handler, which reuses the same
/// [`dispatch_run_octos_session`] business logic, harness-event emission, and
/// `octos_mcp_server_call_total` metric as the legacy JSON-RPC path.
#[derive(Clone)]
pub struct OctosMcpHandler {
    dispatch: Arc<dyn McpSessionDispatch>,
    supervisor: Arc<TaskSupervisor>,
    event_sink: Arc<RwLock<Option<EventSink>>>,
    transport_label: &'static str,
}

impl ServerHandler for OctosMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::new("octos", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Exposes a single tool, `run_octos_session`, that runs a complete octos \
                 session (workspace contract + input to artifact) to completion, including \
                 workspace-contract enforcement. Internal tool calls and progress events \
                 are not streamed to the caller.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: vec![run_octos_session_tool()],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if request.name.as_ref() != RUN_OCTOS_SESSION_TOOL {
            return Err(ErrorData::invalid_params(
                format!(
                    "unknown tool '{}'; this server exposes only '{RUN_OCTOS_SESSION_TOOL}'",
                    request.name
                ),
                None,
            ));
        }

        // Adapt the typed rmcp request into the `{name, arguments}` shape the
        // shared dispatch helper expects, so all transports run identical
        // supervisor / contract / artifact logic.
        let arguments = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| Value::Object(Default::default()));
        let params = json!({ "name": RUN_OCTOS_SESSION_TOOL, "arguments": arguments });

        // Run the session, but abort it if rmcp cancels this request: a client
        // `notifications/cancelled`, a disconnect, or session teardown (e.g. the
        // idle-timeout reaper firing on a pathological multi-hour call). Without
        // this the dispatched agent would keep running with no client to receive
        // its result. `context.ct` descends from the serve-loop token, so it
        // fires on all three; dropping the dispatch future cancels the agent.
        let result = tokio::select! {
            result = dispatch_run_octos_session(&*self.dispatch, &self.supervisor, &params) => result,
            _ = context.ct.cancelled() => Err(McpServerError::SessionFailed(
                "run_octos_session cancelled by the client or session teardown".to_string(),
            )),
        };

        let sink = self.event_sink.read().await.clone();
        emit_call_outcome(&sink, self.transport_label, &result);

        match result {
            Ok(value) => Ok(value_to_call_tool_result(value)),
            Err(err) => Err(mcp_error_to_error_data(err)),
        }
    }
}

/// The single MCP tool advertised by the server, as a typed rmcp [`Tool`].
fn run_octos_session_tool() -> Tool {
    let schema = json!({
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
    });
    let input_schema = schema.as_object().cloned().unwrap_or_default();
    Tool::new(
        RUN_OCTOS_SESSION_TOOL,
        "Run a complete octos session. The caller supplies a workspace contract name and an \
         input payload; octos runs its normal loop to completion (including workspace-contract \
         enforcement) and returns the resulting artifact. Internal tool calls and progress \
         events are not streamed to the caller.",
        input_schema,
    )
}

/// Convert the shared dispatch result (`{content:[{text}], isError}`) into a
/// typed rmcp [`CallToolResult`], preserving the tool-level error flag so the
/// caller's MCP client renders failures with their recovery hints.
fn value_to_call_tool_result(value: Value) -> CallToolResult {
    let text = value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let is_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = vec![Content::text(text)];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// Map a typed [`McpServerError`] onto an rmcp JSON-RPC [`ErrorData`]. Used
/// only for protocol-level failures (bad params, unroutable tool, server
/// fault); session-level failures travel as `CallToolResult::error` so the
/// caller sees the message.
fn mcp_error_to_error_data(err: McpServerError) -> ErrorData {
    match err {
        McpServerError::InvalidParams(msg) => ErrorData::invalid_params(msg, None),
        McpServerError::UnknownTool(name) => {
            ErrorData::invalid_params(format!("unknown tool: {name}"), None)
        }
        McpServerError::ProtocolError(msg) => ErrorData::invalid_request(msg, None),
        McpServerError::SessionFailed(msg) => ErrorData::internal_error(msg, None),
        McpServerError::Unauthorized => ErrorData::invalid_request("authentication required", None),
    }
}

/// Emit the `McpServerCall` harness event and increment
/// `octos_mcp_server_call_total{tool,outcome}` for a completed
/// `run_octos_session` dispatch. Shared by the rmcp `call_tool` handler and
/// the legacy JSON-RPC `handle_tools_call` path so both transports observe the
/// same audit trail.
fn emit_call_outcome(
    sink: &Option<EventSink>,
    transport: &str,
    result: &Result<Value, McpServerError>,
) {
    let (outcome_label, contract_label, error_message) = match result {
        Ok(value) => extract_outcome_from_result(value),
        Err(err) => ("error".to_string(), None, Some(err.to_string())),
    };
    if let Some(sink) = sink {
        let event = HarnessEvent::mcp_server_call(
            format!("mcp:{transport}"),
            TaskId::new().to_string(),
            RUN_OCTOS_SESSION_TOOL,
            caller_id_for_transport(transport),
            transport,
            &outcome_label,
            contract_label,
            error_message,
        );
        (sink)(event);
    }
    counter!(
        "octos_mcp_server_call_total",
        "tool" => RUN_OCTOS_SESSION_TOOL.to_string(),
        "outcome" => outcome_label,
    )
    .increment(1);
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
        serde_json::to_value(&outcome.validator_results)
            .expect("validator outcomes are JSON-serializable"),
    );
    body.insert(
        "cost".into(),
        serde_json::to_value(&outcome.cost).expect("MCP session cost is JSON-serializable"),
    );
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

/// Constant-time comparison of two strings, used by the HTTP bearer-token
/// check (in the CLI's axum middleware) to avoid timing leaks. Public so the
/// serving layer can reuse the same comparison the parser is paired with.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
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

    /// A dispatch that always returns a Ready outcome so the rmcp round-trip
    /// test can assert protocol behavior without running the agent loop.
    struct ReadyDispatch;

    #[async_trait]
    impl McpSessionDispatch for ReadyDispatch {
        async fn run_session(
            &self,
            _contract: &str,
            _input: &Value,
            observer: &dyn SessionLifecycleObserver,
        ) -> Result<McpSessionOutcome, McpServerError> {
            observer.mark_state(TaskLifecycleState::Running);
            observer.mark_state(TaskLifecycleState::Verifying);
            observer.mark_state(TaskLifecycleState::Ready);
            Ok(McpSessionOutcome {
                final_state: TaskLifecycleState::Ready,
                artifact_path: Some("out/deck.pptx".to_string()),
                artifact_content: Some("BYTES".to_string()),
                validator_results: vec![],
                cost: McpSessionCost {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                },
                error: None,
            })
        }
    }

    /// End-to-end proof that the rmcp [`OctosMcpHandler`] speaks the real MCP
    /// protocol: an rmcp client connected over an in-memory duplex completes
    /// the initialize handshake, sees exactly `run_octos_session` via
    /// `tools/list`, and receives a non-error `tools/call` result.
    #[tokio::test]
    async fn serves_run_octos_session_over_real_rmcp_transport() {
        use rmcp::model::{CallToolRequestParams, ClientInfo};
        use rmcp::service::serve_client;

        let server = McpServer::new(Arc::new(ReadyDispatch), Arc::new(TaskSupervisor::new()));
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (server_read, server_write) = tokio::io::split(server_io);

        let server_task =
            tokio::spawn(async move { server.serve_io(server_read, server_write).await });

        let client = serve_client(ClientInfo::default(), (client_read, client_write))
            .await
            .expect("client handshake should complete");

        // tools/list advertises exactly the one session-level tool.
        let tools = client.list_all_tools().await.expect("tools/list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_ref(), RUN_OCTOS_SESSION_TOOL);

        // tools/call routes through the dispatch and returns a non-error bundle.
        let mut param = CallToolRequestParams::new(RUN_OCTOS_SESSION_TOOL);
        param.arguments = json!({ "contract": "coding", "input": {} })
            .as_object()
            .cloned();
        let result = client.call_tool(param).await.expect("tools/call");
        assert_ne!(result.is_error, Some(true));
        assert!(!result.content.is_empty());

        client.cancel().await.ok();
        let _ = server_task.await;
    }

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
            cost: McpSessionCost::default(),
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
            validator_results: Vec::new(),
            cost: McpSessionCost {
                input_tokens: 10,
                ..Default::default()
            },
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
