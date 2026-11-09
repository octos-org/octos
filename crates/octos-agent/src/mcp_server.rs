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
//! Versioned ARC callers may provide a native `input.arc_task`; see
//! `docs/ARC_AGENT_TASK_MCP.md`.
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
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, ListToolsResult,
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

/// Maximum bytes in ONE newline-delimited stdio JSON-RPC request, including
/// params and every other envelope field. Matches the pre-rmcp server cap.
const MAX_MCP_REQUEST_BYTES: usize = 1_048_576;
/// Maximum container/scalar nesting depth of the COMPLETE request value.
const MAX_MCP_REQUEST_DEPTH: usize = 20;

/// Inbound result from the bounded stdio decoder. Limit violations are items,
/// not stream-fatal errors: the transport writes a typed JSON-RPC error and
/// resumes at the next frame.
enum McpInbound {
    Message(Box<rmcp::service::RxJsonRpcMessage<RoleServer>>),
    InvalidRequest(&'static str),
    /// A complete frame was consumed but must not reach rmcp (ignored JSON or
    /// a notification). This is distinct from an incomplete frame: receive
    /// must immediately decode the next buffered frame.
    Skip,
}

/// Newline decoder with a hard byte cap and whole-request JSON depth gate.
/// The oversize discard state is the standard framed-codec pattern: after
/// reporting one violation, bytes through the next newline are discarded
/// without growing an unbounded buffer.
#[derive(Default)]
struct BoundedMcpRequestDecoder {
    next_index: usize,
    is_discarding: bool,
}

impl BoundedMcpRequestDecoder {
    fn decode_line(line: &[u8]) -> Option<McpInbound> {
        let line = line
            .strip_prefix(b"\xEF\xBB\xBF".as_slice())
            .unwrap_or(line);
        let value: Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            Err(error)
                if matches!(
                    error.classify(),
                    serde_json::error::Category::Syntax | serde_json::error::Category::Eof
                ) =>
            {
                return None;
            }
            Err(_) => return Some(McpInbound::InvalidRequest("Invalid request")),
        };
        let is_notification = value.get("method").is_some() && value.get("id").is_none();
        if json_depth(&value) > MAX_MCP_REQUEST_DEPTH {
            if is_notification {
                // JSON-RPC notifications never get a response, even when the
                // payload violates the depth policy. Drop it and resume at
                // the next frame instead of fabricating an error response.
                return None;
            }
            return Some(McpInbound::InvalidRequest(
                "request exceeds maximum JSON depth of 20",
            ));
        }
        match serde_json::from_value(value) {
            Ok(message) => Some(McpInbound::Message(Box::new(message))),
            Err(_) if is_notification => {
                // Preserve rmcp compatibility: unrecognized notifications are
                // ignored and, like every notification, never get a response.
                None
            }
            Err(_) => Some(McpInbound::InvalidRequest("Invalid request")),
        }
    }
}

impl tokio_util::codec::Decoder for BoundedMcpRequestDecoder {
    type Item = McpInbound;
    type Error = std::io::Error;

    fn decode(
        &mut self,
        buf: &mut tokio_util::bytes::BytesMut,
    ) -> std::io::Result<Option<McpInbound>> {
        use tokio_util::bytes::Buf as _;
        loop {
            let read_to = MAX_MCP_REQUEST_BYTES.saturating_add(1).min(buf.len());
            let newline_offset = buf[self.next_index..read_to]
                .iter()
                .position(|byte| *byte == b'\n');
            match (self.is_discarding, newline_offset) {
                (true, Some(offset)) => {
                    buf.advance(offset + self.next_index + 1);
                    self.is_discarding = false;
                    self.next_index = 0;
                }
                (true, None) => {
                    buf.advance(read_to);
                    self.next_index = 0;
                    if buf.is_empty() {
                        return Ok(None);
                    }
                }
                (false, Some(offset)) => {
                    let newline_index = offset + self.next_index;
                    self.next_index = 0;
                    let line = buf.split_to(newline_index + 1);
                    let line = &line[..line.len() - 1];
                    let line = line.strip_suffix(b"\r").unwrap_or(line);
                    if line.is_empty() {
                        continue;
                    }
                    if line.len() > MAX_MCP_REQUEST_BYTES {
                        return Ok(Some(McpInbound::InvalidRequest(
                            "request exceeds 1 MiB limit",
                        )));
                    }
                    return Ok(Some(Self::decode_line(line).unwrap_or(McpInbound::Skip)));
                }
                (false, None) if buf.len() > MAX_MCP_REQUEST_BYTES => {
                    self.is_discarding = true;
                    self.next_index = 0;
                    return Ok(Some(McpInbound::InvalidRequest(
                        "request exceeds 1 MiB limit",
                    )));
                }
                (false, None) => {
                    self.next_index = read_to;
                    return Ok(None);
                }
            }
        }
    }

    fn decode_eof(
        &mut self,
        buf: &mut tokio_util::bytes::BytesMut,
    ) -> std::io::Result<Option<McpInbound>> {
        if let Some(item) = self.decode(buf)? {
            return Ok(Some(item));
        }
        self.next_index = 0;
        if buf.is_empty() || self.is_discarding {
            return Ok(None);
        }
        let line = buf.split_to(buf.len());
        let line = line.strip_suffix(b"\r").unwrap_or(&line);
        if line.is_empty() {
            return Ok(None);
        }
        if line.len() > MAX_MCP_REQUEST_BYTES {
            return Ok(Some(McpInbound::InvalidRequest(
                "request exceeds 1 MiB limit",
            )));
        }
        Ok(Some(Self::decode_line(line).unwrap_or(McpInbound::Skip)))
    }
}

fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => values
            .iter()
            .map(json_depth)
            .max()
            .unwrap_or(0)
            .saturating_add(1),
        Value::Object(values) => values
            .values()
            .map(json_depth)
            .max()
            .unwrap_or(0)
            .saturating_add(1),
        _ => 1,
    }
}

/// rmcp transport with Octos's inbound stdio limits. Outbound framing remains
/// rmcp's own codec; only the previously-unbounded inbound read is replaced.
struct BoundedMcpTransport<R, W> {
    read: tokio_util::codec::FramedRead<R, BoundedMcpRequestDecoder>,
    write: Arc<Mutex<Option<rmcp::transport::async_rw::TransportWriter<RoleServer, W>>>>,
}

impl<R, W> BoundedMcpTransport<R, W>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    fn new(read: R, write: W) -> Self {
        Self {
            read: tokio_util::codec::FramedRead::new(read, BoundedMcpRequestDecoder::default()),
            write: Arc::new(Mutex::new(Some(tokio_util::codec::FramedWrite::new(
                write,
                rmcp::transport::async_rw::JsonRpcMessageCodec::default(),
            )))),
        }
    }

    async fn reject(&mut self, reason: &'static str) -> std::io::Result<()> {
        use futures::SinkExt as _;
        let mut write = self.write.lock().await;
        let Some(write) = write.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "transport is closed",
            ));
        };
        write
            .send(rmcp::service::TxJsonRpcMessage::<RoleServer>::error(
                ErrorData::invalid_request(reason, None),
                None,
            ))
            .await
            .map_err(std::io::Error::from)
    }
}

impl<R, W> rmcp::transport::Transport<RoleServer> for BoundedMcpTransport<R, W>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = std::io::Result<()>> + Send + 'static {
        let write = self.write.clone();
        async move {
            use futures::SinkExt as _;
            let mut write = write.lock().await;
            let Some(write) = write.as_mut() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "transport is closed",
                ));
            };
            write.send(item).await.map_err(std::io::Error::from)
        }
    }

    async fn receive(&mut self) -> Option<rmcp::service::RxJsonRpcMessage<RoleServer>> {
        use futures::StreamExt as _;
        loop {
            match self.read.next().await {
                Some(Ok(McpInbound::Message(message))) => return Some(*message),
                Some(Ok(McpInbound::Skip)) => continue,
                Some(Ok(McpInbound::InvalidRequest(reason))) => {
                    if self.reject(reason).await.is_err() {
                        return None;
                    }
                }
                Some(Err(error)) => {
                    tracing::warn!(%error, "mcp-serve stdio input failed");
                    return None;
                }
                None => return None,
            }
        }
    }

    async fn close(&mut self) -> std::io::Result<()> {
        self.write.lock().await.take();
        Ok(())
    }
}

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
        let transport = BoundedMcpTransport::new(read, write);
        let running = serve_server(handler, transport)
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
    let schema = run_octos_session_schema();
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

fn run_octos_session_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "contract": {
                "type": "string",
                "description": "Workspace contract name (e.g. 'slides_delivery', 'site_delivery', 'coding')."
            },
            "input": {
                "type": "object",
                "description": "Contract-specific session input. Native ARC callers may supply a versioned arc_task.",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Legacy free-form session prompt. Ignored when arc_task is present."
                    },
                    "expected_artifact": {
                        "type": "string",
                        "description": "Expected workspace artifact path."
                    },
                    "artifact_name": {
                        "type": "string",
                        "description": "Workspace-contract artifact name."
                    },
                    "arc_task": crate::arc_task::arc_agent_task_v1_schema()
                },
                "additionalProperties": true
            }
        },
        "required": ["contract", "input"]
    })
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
    let content = vec![ContentBlock::text(text)];
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
            "inputSchema": run_octos_session_schema()
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
