//! MCP-backed sub-agent backends for [`crate::tools::spawn::SpawnTool`].
//!
//! This module lets octos dispatch a task to an external agent that speaks
//! the Model Context Protocol (for example Claude Code via
//! `claude mcp serve`, Codex via `codex mcp serve`, or any conforming
//! hermes/jiuwenclaw runtime). The spawn tool hands the task to the backend
//! via a `tools/call` JSON-RPC request. The sub-agent runs its own tool
//! loop internally, and only the final (contract-gated) artifact is
//! returned to the parent context — the sub-agent's intermediate messages
//! never leak upward.
//!
//! Two transports are supported:
//!
//! - [`StdioMcpAgent`] — spawns a local subprocess and talks JSON-RPC over
//!   stdin/stdout. Applies [`BLOCKED_ENV_VARS`] to the child environment,
//!   wires `kill_on_drop(true)`, and enforces an explicit kill on timeout
//!   so the child is reaped even if its `wait()` future is abandoned.
//! - [`HttpMcpAgent`] — connects to a remote MCP endpoint over HTTPS with
//!   configured connect and read timeouts. Honours SSRF allowlists through
//!   [`crate::tools::ssrf::check_ssrf_with_addrs`] and pins the resolved
//!   DNS addresses on the `reqwest::Client` to prevent DNS rebinding.
//!
//! The dispatch result is surfaced to the runtime as a typed
//! [`crate::harness_events::HarnessEventPayload::SubAgentDispatch`] event,
//! and accounted via the `octos_sub_agent_dispatch_total{backend, outcome}`
//! counter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use metrics::counter;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::warn;

use crate::harness_events::HarnessEventPayload;
use crate::sandbox::BLOCKED_ENV_VARS;
use crate::subprocess_env::{EnvAllowlist, sanitize_command_env, should_forward_env_name};
use crate::tools::ssrf::check_ssrf_with_addrs;

/// Default connect and read timeouts for the HTTP backend. Mirrors the
/// webhook proxy convention of 10 seconds per leg.
pub const DEFAULT_HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_HTTP_READ_TIMEOUT_SECS: u64 = 10;

/// Default wallclock budget for a single `tools/call` dispatch. The
/// backend kills the subprocess and fails the dispatch if the remote agent
/// has not produced a response by this point.
pub const DEFAULT_DISPATCH_TIMEOUT_SECS: u64 = 180;

/// Upper bound on a single JSON-RPC line read from a stdio child. Matches
/// [`crate::mcp`]'s 1 MiB ceiling so oversize frames do not OOM the parent.
const MAX_LINE_BYTES: usize = 1_048_576;

/// Typed configuration for a spawn-backed MCP agent. The caller picks one
/// variant up front; the tool dispatcher selects the matching
/// [`McpAgentBackend`] implementation at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAgentBackendConfig {
    /// Spawn a subprocess that speaks MCP over stdio (e.g. `claude mcp
    /// serve`).
    Local {
        /// Absolute or PATH-resolved executable to invoke.
        cmd: String,
        /// Arguments passed to the child process.
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment variables the child is allowed to see.
        /// [`BLOCKED_ENV_VARS`] always win — any name in that list is
        /// stripped regardless of this allowlist.
        #[serde(default)]
        env: HashMap<String, String>,
        /// Per-dispatch wallclock budget in seconds. Defaults to
        /// [`DEFAULT_DISPATCH_TIMEOUT_SECS`].
        #[serde(default)]
        dispatch_timeout_secs: Option<u64>,
    },
    /// Connect to a remote MCP endpoint over HTTPS.
    Remote {
        /// Fully qualified URL of the remote endpoint.
        url: String,
        /// Optional authorization header value (copied verbatim into an
        /// `Authorization:` header).
        #[serde(default)]
        auth_header: Option<String>,
        /// Additional headers to forward.
        #[serde(default)]
        extra_headers: HashMap<String, String>,
        /// Connect timeout in seconds.
        #[serde(default)]
        connect_timeout_secs: Option<u64>,
        /// Read timeout in seconds.
        #[serde(default)]
        read_timeout_secs: Option<u64>,
        /// Per-dispatch wallclock budget in seconds.
        #[serde(default)]
        dispatch_timeout_secs: Option<u64>,
    },
}

impl McpAgentBackendConfig {
    /// Stable backend label used in logs, events, and the
    /// `octos_sub_agent_dispatch_total` counter. One of `"local"` or
    /// `"remote"`.
    pub fn backend_label(&self) -> &'static str {
        match self {
            Self::Local { .. } => "local",
            Self::Remote { .. } => "remote",
        }
    }

    /// Human-readable endpoint ID (e.g. the command name or URL). Used in
    /// dispatch events so operators can tell backends apart.
    pub fn endpoint_label(&self) -> String {
        match self {
            Self::Local { cmd, .. } => cmd.clone(),
            Self::Remote { url, .. } => url.clone(),
        }
    }

    /// Per-dispatch wallclock budget resolved against
    /// [`DEFAULT_DISPATCH_TIMEOUT_SECS`]. Used by the backend
    /// implementations and exposed for tests that want to assert default
    /// timeouts without re-deriving the fallback.
    pub fn dispatch_timeout(&self) -> Duration {
        let secs = match self {
            Self::Local {
                dispatch_timeout_secs,
                ..
            } => dispatch_timeout_secs,
            Self::Remote {
                dispatch_timeout_secs,
                ..
            } => dispatch_timeout_secs,
        };
        Duration::from_secs(secs.unwrap_or(DEFAULT_DISPATCH_TIMEOUT_SECS))
    }
}

/// Outcome label for a dispatch attempt. Stable strings — extending this
/// requires updating the `octos_sub_agent_dispatch_total` counter
/// documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Remote agent returned a well-formed response with a completed
    /// artifact.
    Success,
    /// The remote agent returned a JSON-RPC error or a `content` array
    /// flagged with `isError: true`.
    RemoteError,
    /// The dispatch exceeded its wallclock budget and the child was
    /// killed (stdio) or the HTTP request was aborted.
    Timeout,
    /// Spawn/connect failure before the remote agent saw the request.
    TransportError,
    /// Response body was malformed JSON or missing required fields.
    ProtocolError,
    /// URL failed the SSRF allowlist (remote backend only).
    SsrfBlocked,
}

impl DispatchOutcome {
    /// Stable label used in metrics and typed harness events.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::RemoteError => "remote_error",
            Self::Timeout => "timeout",
            Self::TransportError => "transport_error",
            Self::ProtocolError => "protocol_error",
            Self::SsrfBlocked => "ssrf_blocked",
        }
    }
}

/// Structured payload returned by a backend after a single dispatch. The
/// caller translates this into a `tools::ToolResult` and a typed harness
/// event.
#[derive(Debug, Clone)]
pub struct DispatchResponse {
    pub outcome: DispatchOutcome,
    /// Plain-text summary of the remote agent's final output. Multi-part
    /// MCP `content` arrays are joined newline-separated.
    pub output: String,
    /// Artifact paths the remote agent wants surfaced to the parent
    /// context. Each path is returned verbatim; the caller is expected
    /// to fold these through the workspace contract.
    pub files_to_send: Vec<PathBuf>,
    /// Optional error message. Populated for every non-`Success` outcome.
    pub error: Option<String>,
}

impl DispatchResponse {
    fn success(output: String, files_to_send: Vec<PathBuf>) -> Self {
        Self {
            outcome: DispatchOutcome::Success,
            output,
            files_to_send,
            error: None,
        }
    }

    fn failure(outcome: DispatchOutcome, error: impl Into<String>) -> Self {
        let message = error.into();
        Self {
            outcome,
            output: message.clone(),
            files_to_send: Vec::new(),
            error: Some(message),
        }
    }
}

/// A request dispatched to an MCP-backed sub-agent. The backend forwards
/// this verbatim as the `arguments` payload of a `tools/call` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchRequest {
    /// MCP tool name to invoke on the remote agent. Typically matches
    /// the agent's own "run a task" tool (for example
    /// `claude_code/run_task`).
    pub tool_name: String,
    /// Task prompt or structured instruction payload the remote agent
    /// consumes. Opaque to this module.
    pub task: serde_json::Value,
}

/// Trait implemented by each transport backend. Implementations MUST be
/// cancel-safe — the caller wraps the dispatch in `tokio::time::timeout`
/// and expects the backend to abort cleanly on drop.
#[async_trait]
pub trait McpAgentBackend: Send + Sync {
    /// Stable label for the transport (`"local"` / `"remote"`). Used in
    /// metrics and events.
    fn backend_label(&self) -> &'static str;

    /// Human-readable endpoint identifier (command, URL, ...).
    fn endpoint_label(&self) -> String;

    /// Dispatch `request` to the remote agent and await the final
    /// response. Returns a [`DispatchResponse`] even for failure modes —
    /// callers inspect [`DispatchResponse::outcome`] to pick the right
    /// event payload.
    async fn dispatch(&self, request: DispatchRequest) -> DispatchResponse;
}

// ── Stdio backend ─────────────────────────────────────────────────────────

/// Subprocess-based MCP agent. Owns the [`Command`] template for the
/// child and re-spawns a fresh process per dispatch so failures stay
/// scoped.
pub struct StdioMcpAgent {
    cmd: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: Option<PathBuf>,
    dispatch_timeout: Duration,
}

impl StdioMcpAgent {
    /// Construct a stdio backend from typed config.
    pub fn from_config(config: &McpAgentBackendConfig) -> Result<Self> {
        let McpAgentBackendConfig::Local {
            cmd,
            args,
            env,
            dispatch_timeout_secs,
        } = config
        else {
            eyre::bail!("StdioMcpAgent requires a Local backend config");
        };
        if cmd.trim().is_empty() {
            eyre::bail!("stdio MCP agent requires a non-empty command");
        }
        Ok(Self {
            cmd: cmd.clone(),
            args: args.clone(),
            env: env.clone(),
            cwd: None,
            dispatch_timeout: Duration::from_secs(
                dispatch_timeout_secs.unwrap_or(DEFAULT_DISPATCH_TIMEOUT_SECS),
            ),
        })
    }

    /// Set the working directory for spawned children.
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Override the dispatch timeout (useful in tests).
    pub fn with_dispatch_timeout(mut self, timeout: Duration) -> Self {
        self.dispatch_timeout = timeout;
        self
    }

    fn build_command(&self) -> Command {
        let mut cmd = Command::new(&self.cmd);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }

        // Scrub parent-inherited env down to a safe allowlist before layering
        // on the caller-configured env. Strip [`BLOCKED_ENV_VARS`] last so
        // they win even if the caller tries to reintroduce them.
        let allowlist = EnvAllowlist::from_names(self.env.keys().map(|key| key.as_str()));
        sanitize_command_env(&mut cmd, &allowlist);

        for (key, value) in &self.env {
            if BLOCKED_ENV_VARS
                .iter()
                .any(|blocked| key.eq_ignore_ascii_case(blocked))
            {
                warn!(
                    key = key.as_str(),
                    "blocked dangerous MCP sub-agent environment variable"
                );
                continue;
            }
            if !should_forward_env_name(key, &allowlist) {
                warn!(
                    key = key.as_str(),
                    "blocked non-allowlisted MCP sub-agent environment variable"
                );
                continue;
            }
            cmd.env(key, value);
        }

        for blocked in BLOCKED_ENV_VARS {
            cmd.env_remove(blocked);
        }

        cmd.kill_on_drop(true);
        cmd
    }

    async fn dispatch_inner(&self, request: DispatchRequest) -> DispatchResponse {
        let mut command = self.build_command();
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return DispatchResponse::failure(
                    DispatchOutcome::TransportError,
                    format!("failed to spawn MCP sub-agent '{}': {error}", self.cmd),
                );
            }
        };

        match tokio::time::timeout(
            self.dispatch_timeout,
            run_stdio_dispatch(child, request.clone()),
        )
        .await
        {
            Ok(response) => response,
            Err(_) => DispatchResponse::failure(
                DispatchOutcome::Timeout,
                format!(
                    "MCP sub-agent '{}' did not respond within {:?}",
                    self.cmd, self.dispatch_timeout
                ),
            ),
        }
    }
}

#[async_trait]
impl McpAgentBackend for StdioMcpAgent {
    fn backend_label(&self) -> &'static str {
        "local"
    }

    fn endpoint_label(&self) -> String {
        self.cmd.clone()
    }

    async fn dispatch(&self, request: DispatchRequest) -> DispatchResponse {
        self.dispatch_inner(request).await
    }
}

/// Drive a single dispatch against `child`, kill-on-timeout semantics are
/// provided by the caller via [`tokio::time::timeout`] and the
/// `kill_on_drop(true)` flag set on the command. This function still
/// explicitly kills the child on error paths that reach `ChildHandle::
/// terminate` so any file descriptors held by buffered readers are
/// released promptly.
async fn run_stdio_dispatch(mut child: Child, request: DispatchRequest) -> DispatchResponse {
    let guard = ChildGuard::new(&mut child);
    let stdin = match guard.child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            return DispatchResponse::failure(
                DispatchOutcome::TransportError,
                "MCP sub-agent stdin unavailable",
            );
        }
    };
    let stdout = match guard.child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            return DispatchResponse::failure(
                DispatchOutcome::TransportError,
                "MCP sub-agent stdout unavailable",
            );
        }
    };
    let reader = BufReader::new(stdout);

    match perform_stdio_handshake_and_call(stdin, reader, request).await {
        Ok(response) => {
            // Handshake succeeded — let the child terminate gracefully. We
            // still kill via the guard to avoid lingering idle children.
            response
        }
        Err((outcome, error)) => DispatchResponse::failure(outcome, error),
    }
}

struct ChildGuard<'a> {
    child: &'a mut Child,
}

impl<'a> ChildGuard<'a> {
    fn new(child: &'a mut Child) -> Self {
        Self { child }
    }
}

impl Drop for ChildGuard<'_> {
    fn drop(&mut self) {
        // start_kill sends SIGKILL on Unix / TerminateProcess on Windows
        // immediately; kill_on_drop(true) on the Command ensures the
        // async reaper runs after we return.
        let _ = self.child.start_kill();
    }
}

async fn perform_stdio_handshake_and_call(
    mut stdin: ChildStdin,
    mut reader: BufReader<ChildStdout>,
    request: DispatchRequest,
) -> std::result::Result<DispatchResponse, (DispatchOutcome, String)> {
    send_json_rpc(
        &mut stdin,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "octos", "version": env!("CARGO_PKG_VERSION")}
        }),
    )
    .await
    .map_err(|error| {
        (
            DispatchOutcome::TransportError,
            format!("MCP initialize write failed: {error}"),
        )
    })?;
    let _init = read_json_rpc_response(&mut reader).await.map_err(|error| {
        (
            DispatchOutcome::ProtocolError,
            format!("MCP initialize response invalid: {error}"),
        )
    })?;

    send_json_rpc(
        &mut stdin,
        2,
        "tools/call",
        serde_json::json!({
            "name": request.tool_name,
            "arguments": request.task,
        }),
    )
    .await
    .map_err(|error| {
        (
            DispatchOutcome::TransportError,
            format!("MCP tools/call write failed: {error}"),
        )
    })?;

    let response = read_json_rpc_response(&mut reader).await.map_err(|error| {
        (
            DispatchOutcome::ProtocolError,
            format!("MCP tools/call response invalid: {error}"),
        )
    })?;

    Ok(parse_tools_call_response(response))
}

async fn send_json_rpc(
    stdin: &mut ChildStdin,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> std::io::Result<()> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_string(&request).map_err(std::io::Error::other)?;
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

async fn read_json_rpc_response(
    reader: &mut BufReader<ChildStdout>,
) -> std::result::Result<serde_json::Value, String> {
    let line = read_line_limited(reader, MAX_LINE_BYTES)
        .await
        .map_err(|error| error.to_string())?;
    let envelope: serde_json::Value = serde_json::from_str(&line)
        .map_err(|error| format!("invalid JSON-RPC response: {error}"))?;

    if let Some(err) = envelope.get("error").and_then(|v| v.as_object()) {
        let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-32603);
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!("remote error {code}: {message}"));
    }

    envelope
        .get("result")
        .cloned()
        .ok_or_else(|| "JSON-RPC response missing 'result'".to_string())
}

async fn read_line_limited(reader: &mut BufReader<ChildStdout>, limit: usize) -> Result<String> {
    let mut buf = Vec::with_capacity(4096);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            eyre::bail!("MCP sub-agent closed stdout before responding");
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=pos]);
            reader.consume(pos + 1);
            break;
        }
        if buf.len() + available.len() > limit {
            eyre::bail!("MCP response exceeds {} bytes", limit);
        }
        let len = available.len();
        buf.extend_from_slice(available);
        reader.consume(len);
    }
    String::from_utf8(buf).wrap_err("MCP response is not valid UTF-8")
}

fn parse_tools_call_response(result: serde_json::Value) -> DispatchResponse {
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let output = match result.get("content").and_then(|v| v.as_array()) {
        Some(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        None => result.to_string(),
    };

    let files_to_send = match result.get("files_to_send").and_then(|v| v.as_array()) {
        Some(items) => items
            .iter()
            .filter_map(|value| value.as_str().map(PathBuf::from))
            .collect(),
        None => Vec::new(),
    };

    if is_error {
        return DispatchResponse {
            outcome: DispatchOutcome::RemoteError,
            output: output.clone(),
            files_to_send: Vec::new(),
            error: Some(output),
        };
    }

    DispatchResponse::success(output, files_to_send)
}

// ── HTTP backend ──────────────────────────────────────────────────────────

/// Remote MCP agent reached over HTTPS. Each [`Self::dispatch`] call
/// opens a fresh JSON-RPC request with connect and read timeouts
/// enforced via `reqwest::ClientBuilder`.
pub struct HttpMcpAgent {
    url: String,
    auth_header: Option<String>,
    extra_headers: HashMap<String, String>,
    connect_timeout: Duration,
    read_timeout: Duration,
    dispatch_timeout: Duration,
    /// Test-only bypass for SSRF allowlisting. Real callers must never
    /// enable this — it exists so integration tests can stand up a
    /// loopback peer without flaking on the production SSRF guard.
    allow_loopback_for_tests: bool,
}

impl HttpMcpAgent {
    /// Construct an HTTP backend from typed config.
    pub fn from_config(config: &McpAgentBackendConfig) -> Result<Self> {
        let McpAgentBackendConfig::Remote {
            url,
            auth_header,
            extra_headers,
            connect_timeout_secs,
            read_timeout_secs,
            dispatch_timeout_secs,
        } = config
        else {
            eyre::bail!("HttpMcpAgent requires a Remote backend config");
        };
        if url.trim().is_empty() {
            eyre::bail!("HTTP MCP agent requires a non-empty URL");
        }
        let connect_timeout =
            Duration::from_secs(connect_timeout_secs.unwrap_or(DEFAULT_HTTP_CONNECT_TIMEOUT_SECS));
        let read_timeout =
            Duration::from_secs(read_timeout_secs.unwrap_or(DEFAULT_HTTP_READ_TIMEOUT_SECS));
        let dispatch_timeout =
            Duration::from_secs(dispatch_timeout_secs.unwrap_or(DEFAULT_DISPATCH_TIMEOUT_SECS));
        Ok(Self {
            url: url.clone(),
            auth_header: auth_header.clone(),
            extra_headers: extra_headers.clone(),
            connect_timeout,
            read_timeout,
            dispatch_timeout,
            allow_loopback_for_tests: false,
        })
    }

    /// Override the dispatch timeout (used in tests).
    pub fn with_dispatch_timeout(mut self, timeout: Duration) -> Self {
        self.dispatch_timeout = timeout;
        self
    }

    /// Test-only escape hatch that lets integration tests stand up a
    /// loopback HTTP peer without tripping the SSRF guard. The
    /// production config path never takes this branch — callers have to
    /// opt in explicitly, and the setter is gated behind a `#[doc(hidden)]`
    /// marker so it does not appear in rustdoc. Using it in production
    /// code is a configuration mistake.
    #[doc(hidden)]
    pub fn with_loopback_allowed_for_tests(mut self) -> Self {
        self.allow_loopback_for_tests = true;
        self
    }

    async fn dispatch_inner(&self, request: DispatchRequest) -> DispatchResponse {
        // SSRF is enforced at the URL layer so the connect timeout does not
        // protect private endpoints by accident.
        let resolved_addrs = match check_ssrf_with_addrs(&self.url).await {
            Ok(result) => result.resolved_addrs,
            Err(message) => {
                if self.allow_loopback_for_tests {
                    // Loopback fallback used only by the integration test
                    // harness. Empty resolved_addrs lets reqwest fall
                    // back to its own DNS resolution — safe for
                    // 127.0.0.1:port targets under `cargo test`.
                    Vec::new()
                } else {
                    return DispatchResponse::failure(
                        DispatchOutcome::SsrfBlocked,
                        format!("MCP remote endpoint blocked by SSRF policy: {message}"),
                    );
                }
            }
        };

        let parsed = match reqwest::Url::parse(&self.url) {
            Ok(url) => url,
            Err(error) => {
                return DispatchResponse::failure(
                    DispatchOutcome::TransportError,
                    format!("invalid MCP remote URL '{}': {error}", self.url),
                );
            }
        };
        let host = match parsed.host_str() {
            Some(host) => host.to_string(),
            None => {
                return DispatchResponse::failure(
                    DispatchOutcome::TransportError,
                    format!("MCP remote URL '{}' is missing host", self.url),
                );
            }
        };

        let mut builder = reqwest::Client::builder()
            .connect_timeout(self.connect_timeout)
            .read_timeout(self.read_timeout);
        for addr in &resolved_addrs {
            builder = builder.resolve(&host, *addr);
        }
        let client = match builder.build() {
            Ok(client) => client,
            Err(error) => {
                return DispatchResponse::failure(
                    DispatchOutcome::TransportError,
                    format!("failed to build HTTPS client: {error}"),
                );
            }
        };

        match tokio::time::timeout(self.dispatch_timeout, self.send_request(&client, &request))
            .await
        {
            Ok(response) => response,
            Err(_) => DispatchResponse::failure(
                DispatchOutcome::Timeout,
                format!(
                    "MCP remote endpoint '{}' did not respond within {:?}",
                    self.url, self.dispatch_timeout
                ),
            ),
        }
    }

    async fn send_request(
        &self,
        client: &reqwest::Client,
        request: &DispatchRequest,
    ) -> DispatchResponse {
        // JSON-RPC body — same shape as the stdio path so the remote
        // agent's dispatcher can treat both transports uniformly.
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": request.tool_name,
                "arguments": request.task,
            },
        });

        let mut req = client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json");
        if let Some(header) = &self.auth_header {
            req = req.header("Authorization", header.as_str());
        }
        for (key, value) in &self.extra_headers {
            req = req.header(key.as_str(), value.as_str());
        }

        let resp = match req.json(&body).send().await {
            Ok(resp) => resp,
            Err(error) => {
                let outcome = if error.is_timeout() {
                    DispatchOutcome::Timeout
                } else {
                    DispatchOutcome::TransportError
                };
                return DispatchResponse::failure(
                    outcome,
                    format!("MCP remote send failed: {error}"),
                );
            }
        };

        let status = resp.status();
        let text = match resp.text().await {
            Ok(text) => text,
            Err(error) => {
                return DispatchResponse::failure(
                    DispatchOutcome::TransportError,
                    format!("MCP remote read failed: {error}"),
                );
            }
        };

        if !status.is_success() {
            return DispatchResponse::failure(
                DispatchOutcome::RemoteError,
                format!("MCP remote HTTP {status}: {text}"),
            );
        }

        let envelope: serde_json::Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                return DispatchResponse::failure(
                    DispatchOutcome::ProtocolError,
                    format!("invalid JSON-RPC envelope: {error}"),
                );
            }
        };

        if let Some(err) = envelope.get("error").and_then(|v| v.as_object()) {
            let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-32603);
            let message = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return DispatchResponse::failure(
                DispatchOutcome::RemoteError,
                format!("remote error {code}: {message}"),
            );
        }

        match envelope.get("result").cloned() {
            Some(result) => parse_tools_call_response(result),
            None => DispatchResponse::failure(
                DispatchOutcome::ProtocolError,
                "JSON-RPC response missing 'result'",
            ),
        }
    }
}

#[async_trait]
impl McpAgentBackend for HttpMcpAgent {
    fn backend_label(&self) -> &'static str {
        "remote"
    }

    fn endpoint_label(&self) -> String {
        self.url.clone()
    }

    async fn dispatch(&self, request: DispatchRequest) -> DispatchResponse {
        self.dispatch_inner(request).await
    }
}

// ── Dispatcher shim ───────────────────────────────────────────────────────

/// Outcome emitted alongside the structured harness event after a dispatch.
#[derive(Debug, Clone)]
pub struct DispatchEventSummary {
    pub backend: String,
    pub endpoint: String,
    pub outcome: String,
}

/// Record a dispatch attempt against the `octos_sub_agent_dispatch_total`
/// counter. Stable label set: `backend` (`"local"` | `"remote"`) and
/// `outcome` (one of [`DispatchOutcome::as_str`]).
pub fn record_dispatch(backend: &str, outcome: DispatchOutcome) {
    counter!(
        "octos_sub_agent_dispatch_total",
        "backend" => backend.to_string(),
        "outcome" => outcome.as_str().to_string()
    )
    .increment(1);
}

/// Build a typed [`HarnessEventPayload::SubAgentDispatch`] payload from a
/// completed dispatch. The caller is expected to wrap this in a full
/// [`crate::harness_events::HarnessEvent`] before writing to the sink.
pub fn build_dispatch_event_payload(
    session_id: impl Into<String>,
    task_id: impl Into<String>,
    workflow: Option<impl Into<String>>,
    phase: Option<impl Into<String>>,
    backend: &dyn McpAgentBackend,
    response: &DispatchResponse,
) -> HarnessEventPayload {
    HarnessEventPayload::SubAgentDispatch {
        data: crate::harness_events::HarnessSubAgentDispatchEvent {
            schema_version: crate::abi_schema::SUB_AGENT_DISPATCH_SCHEMA_VERSION,
            session_id: session_id.into(),
            task_id: task_id.into(),
            workflow: workflow.map(Into::into),
            phase: phase.map(Into::into),
            backend: backend.backend_label().to_string(),
            endpoint: backend.endpoint_label(),
            outcome: response.outcome.as_str().to_string(),
            message: response.error.clone(),
            extra: HashMap::new(),
        },
    }
}

/// Construct the correct [`McpAgentBackend`] implementation from a typed
/// config. Keeps the SpawnTool construction site declarative.
pub fn build_backend_from_config(
    config: &McpAgentBackendConfig,
    cwd: Option<&Path>,
) -> Result<Arc<dyn McpAgentBackend>> {
    match config {
        McpAgentBackendConfig::Local { .. } => {
            let mut backend = StdioMcpAgent::from_config(config)?;
            if let Some(cwd) = cwd {
                backend = backend.with_cwd(cwd.to_path_buf());
            }
            Ok(Arc::new(backend))
        }
        McpAgentBackendConfig::Remote { .. } => Ok(Arc::new(HttpMcpAgent::from_config(config)?)),
    }
}

/// Serialize-safe container around a sharable backend so
/// [`crate::tools::spawn::SpawnTool`] can hold one without leaking the
/// trait object to downstream modules. The boxed form lets us tweak the
/// transport in tests.
pub type SharedBackend = Arc<dyn McpAgentBackend>;

/// Helper used by `SpawnTool` to perform a single dispatch, record the
/// metric, and return a [`DispatchEventSummary`] for the caller to fold
/// into a typed harness event.
pub async fn dispatch_with_metrics(
    backend: &dyn McpAgentBackend,
    request: DispatchRequest,
) -> (DispatchResponse, DispatchEventSummary) {
    let response = backend.dispatch(request).await;
    record_dispatch(backend.backend_label(), response.outcome);
    let summary = DispatchEventSummary {
        backend: backend.backend_label().to_string(),
        endpoint: backend.endpoint_label(),
        outcome: response.outcome.as_str().to_string(),
    };
    (response, summary)
}

/// Convenience: build a `Mutex`-guarded `Command`-like shim so callers
/// that want to re-use a single subprocess can opt-in. Default behaviour
/// spawns a fresh child per dispatch, which keeps dispatch attempts
/// independent (a crash in one attempt cannot leak state into another).
#[allow(dead_code)]
pub type BackendMutex = Mutex<Arc<dyn McpAgentBackend>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_label_round_trips() {
        let local = McpAgentBackendConfig::Local {
            cmd: "claude".into(),
            args: vec!["mcp".into(), "serve".into()],
            env: HashMap::new(),
            dispatch_timeout_secs: Some(5),
        };
        let remote = McpAgentBackendConfig::Remote {
            url: "https://example.com/mcp".into(),
            auth_header: Some("Bearer token".into()),
            extra_headers: HashMap::new(),
            connect_timeout_secs: None,
            read_timeout_secs: None,
            dispatch_timeout_secs: None,
        };
        assert_eq!(local.backend_label(), "local");
        assert_eq!(remote.backend_label(), "remote");
        assert_eq!(local.endpoint_label(), "claude");
        assert_eq!(remote.endpoint_label(), "https://example.com/mcp");
        assert_eq!(local.dispatch_timeout(), Duration::from_secs(5));
        assert_eq!(
            remote.dispatch_timeout(),
            Duration::from_secs(DEFAULT_DISPATCH_TIMEOUT_SECS)
        );
    }

    #[test]
    fn dispatch_outcome_labels_stable() {
        assert_eq!(DispatchOutcome::Success.as_str(), "success");
        assert_eq!(DispatchOutcome::RemoteError.as_str(), "remote_error");
        assert_eq!(DispatchOutcome::Timeout.as_str(), "timeout");
        assert_eq!(DispatchOutcome::TransportError.as_str(), "transport_error");
        assert_eq!(DispatchOutcome::ProtocolError.as_str(), "protocol_error");
        assert_eq!(DispatchOutcome::SsrfBlocked.as_str(), "ssrf_blocked");
    }

    #[test]
    fn parse_tools_call_extracts_text_and_files() {
        let result = serde_json::json!({
            "content": [{"type": "text", "text": "ok"}, {"type": "text", "text": "done"}],
            "files_to_send": ["/tmp/out.md"],
        });
        let response = parse_tools_call_response(result);
        assert_eq!(response.outcome, DispatchOutcome::Success);
        assert_eq!(response.output, "ok\ndone");
        assert_eq!(response.files_to_send, vec![PathBuf::from("/tmp/out.md")]);
    }

    #[test]
    fn parse_tools_call_surfaces_remote_error_flag() {
        let result = serde_json::json!({
            "content": [{"type": "text", "text": "remote rejected"}],
            "isError": true,
        });
        let response = parse_tools_call_response(result);
        assert_eq!(response.outcome, DispatchOutcome::RemoteError);
        assert_eq!(response.output, "remote rejected");
        assert!(response.error.is_some());
    }

    #[test]
    fn stdio_config_rejects_empty_command() {
        let bad = McpAgentBackendConfig::Local {
            cmd: "   ".into(),
            args: vec![],
            env: HashMap::new(),
            dispatch_timeout_secs: None,
        };
        assert!(StdioMcpAgent::from_config(&bad).is_err());
    }

    #[test]
    fn http_config_rejects_empty_url() {
        let bad = McpAgentBackendConfig::Remote {
            url: "".into(),
            auth_header: None,
            extra_headers: HashMap::new(),
            connect_timeout_secs: None,
            read_timeout_secs: None,
            dispatch_timeout_secs: None,
        };
        assert!(HttpMcpAgent::from_config(&bad).is_err());
    }

    #[test]
    fn build_backend_routes_variant_to_impl() {
        let local = McpAgentBackendConfig::Local {
            cmd: "claude".into(),
            args: vec![],
            env: HashMap::new(),
            dispatch_timeout_secs: None,
        };
        let backend = build_backend_from_config(&local, None).unwrap();
        assert_eq!(backend.backend_label(), "local");

        let remote = McpAgentBackendConfig::Remote {
            url: "https://example.com/mcp".into(),
            auth_header: None,
            extra_headers: HashMap::new(),
            connect_timeout_secs: None,
            read_timeout_secs: None,
            dispatch_timeout_secs: None,
        };
        let backend = build_backend_from_config(&remote, None).unwrap();
        assert_eq!(backend.backend_label(), "remote");
    }

    #[test]
    fn dispatch_response_failure_carries_message() {
        let response =
            DispatchResponse::failure(DispatchOutcome::Timeout, "slow agent".to_string());
        assert_eq!(response.outcome, DispatchOutcome::Timeout);
        assert_eq!(response.output, "slow agent");
        assert_eq!(response.error.as_deref(), Some("slow agent"));
        assert!(response.files_to_send.is_empty());
    }
}
