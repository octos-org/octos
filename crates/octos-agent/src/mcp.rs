//! MCP (Model Context Protocol) client for external tool integration.
//!
//! Backed by the official [`rmcp`] SDK. Supports three transports:
//! - **stdio**: spawns MCP servers as child processes (JSON-RPC over stdin/stdout).
//! - **streamable-HTTP**: connects to a remote MCP server URL (optionally with a
//!   static bearer token via a configured `Authorization` header).
//! - **streamable-HTTP + OAuth 2.1**: for `oauth: true` servers, using tokens
//!   obtained by `octos mcp login` and stored in the OS keyring (see
//!   [`crate::mcp_auth`]).
//!
//! rmcp performs the full MCP lifecycle handshake (`initialize` +
//! `notifications/initialized`), negotiates the protocol version, and multiplexes
//! concurrent requests over a single connection with id routing — fixing the
//! hand-rolled client's spec gaps (missing `initialized`, hardcoded version,
//! one-line-per-request desync).

use std::collections::HashMap;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use rmcp::model::{CallToolRequestParams, ClientInfo, Implementation};
use rmcp::service::{RoleClient, RunningService, serve_client};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::time::timeout;
use tracing::{info, warn};

use crate::subprocess_env::{EnvAllowlist, sanitize_command_env, should_forward_env_name};
use crate::tools::{Tool, ToolRegistry, ToolResult};

/// Cap on a single MCP stdio frame (one newline-delimited JSON-RPC message).
/// R-12: rmcp's child-process transport reads frames with an unbounded
/// `read_until`, so a hostile or broken server could grow memory without
/// bound before schema validation. Sized to carry the base64 form of the
/// media budget `mcp_content.rs` accepts, plus 1 MiB of JSON envelope; an
/// oversized frame fails the transport loudly instead of being truncated,
/// which would silently corrupt the protocol.
pub(crate) const MCP_MAX_FRAME_BYTES: usize =
    (octos_llm::vision::MAX_MEDIA_BYTES as usize).div_ceil(3) * 4 + 1_048_576;

/// Bounded line reader: counts bytes since the last newline and fails the
/// read once a frame grows past [`MCP_MAX_FRAME_BYTES`] (R-12). Generic over
/// the inner reader so the cap logic is unit-testable without a process.
struct BoundedLineRead<R: AsyncRead> {
    inner: Pin<Box<R>>,
    line_bytes: usize,
}

impl<R: AsyncRead> BoundedLineRead<R> {
    fn new(inner: R) -> Self {
        Self {
            inner: Box::pin(inner),
            line_bytes: 0,
        }
    }
}

impl<R: AsyncRead> AsyncRead for BoundedLineRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        match self.inner.as_mut().poll_read(cx, buf) {
            std::task::Poll::Ready(Ok(())) => {
                let new = &buf.filled()[before..];
                let mut count = self.line_bytes;
                for byte in new {
                    count = if *byte == b'\n' { 0 } else { count + 1 };
                    if count > MCP_MAX_FRAME_BYTES {
                        return std::task::Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "oversized MCP frame: >{MCP_MAX_FRAME_BYTES} bytes without a \
                                 newline; terminating the transport"
                            ),
                        )));
                    }
                }
                self.line_bytes = count;
                std::task::Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// Bounded stdout wrapper for an MCP stdio child: the frame cap above, plus
/// ownership of the child handle so `kill_on_drop` still terminates the
/// server when the transport drops.
struct BoundedChildStdout {
    /// Kept only for ownership: dropping the child (kill_on_drop) terminates
    /// the server when the transport is retired.
    _child: tokio::process::Child,
    inner: BoundedLineRead<tokio::process::ChildStdout>,
}

impl AsyncRead for BoundedChildStdout {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[path = "mcp_content.rs"]
mod content;

#[cfg(test)]
#[path = "mcp_wire_tests.rs"]
mod wire_tests;

/// A live rmcp client session (any transport). Kept alive behind an `Arc`; when
/// the last reference drops, rmcp closes the transport and (stdio) reaps the
/// child via `kill_on_drop`. `sanitize_command_env` internally strips the shared
/// `BLOCKED_ENV_VARS` from every spawned stdio server.
pub(crate) type McpService = Arc<RunningService<RoleClient, ClientInfo>>;

/// How long to wait for the MCP `initialize` handshake before giving up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a single `tools/call` may run before it is cancelled.
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum nesting depth for MCP tool input schemas.
const MAX_SCHEMA_DEPTH: usize = 10;
/// Maximum serialized size of an MCP tool input schema (64 KB).
const MAX_SCHEMA_SIZE: usize = 65_536;

/// Configuration for a single MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Stable profile-scoped connector id. Legacy entries without an id use
    /// their display name until a management mutation assigns one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Human-readable connector name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Disabled connectors remain configured but are never spawned/connected.
    #[serde(default = "default_mcp_enabled")]
    pub enabled: bool,
    /// Operator allowlist entry that authorized this stdio configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowlist_entry_id: Option<String>,
    /// Stdio transport: command to spawn.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// HTTP transport: URL of the MCP server endpoint.
    #[serde(default)]
    pub url: Option<String>,
    /// HTTP transport: additional headers (e.g. `Authorization` for a static
    /// bearer token). Ignored when `oauth` is set.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// HTTP transport: perform the OAuth 2.1 authorization-code flow against
    /// this server. Tokens are obtained interactively via `octos mcp login`
    /// and loaded from the OS keyring at runtime. Requires `url`.
    #[serde(default)]
    pub oauth: bool,
    /// OAuth scopes to request during `octos mcp login` (server-specific).
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Optional override for the M8.8 concurrency class assigned to every
    /// tool exposed by this server.
    ///
    /// Accepted values: `"safe"`, `"exclusive"` (case-insensitive). Absent →
    /// `Safe` (read-only common case). Unknown values resolve to the
    /// conservative `Exclusive` side (fail-safe: a typo must not silently
    /// downgrade enforcement).
    #[serde(default)]
    pub concurrency_class: Option<String>,
}

fn default_mcp_enabled() -> bool {
    true
}

impl McpServerConfig {
    /// Stable id for management/status APIs. Legacy entries fall back to the
    /// display name and are assigned an id on the next management mutation.
    pub fn connector_id(&self) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| self.display_name().to_string())
    }

    pub fn transport_name(&self) -> &'static str {
        if self.url.is_some() { "http" } else { "stdio" }
    }

    /// Resolve the configured concurrency class for tools spawned by this
    /// server. Absent/`"safe"` → `Safe`; `"exclusive"` → `Exclusive`; unknown
    /// → `Exclusive` (fail-safe).
    pub fn resolved_concurrency_class(&self) -> crate::tools::ConcurrencyClass {
        match self
            .concurrency_class
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None | Some("safe") => crate::tools::ConcurrencyClass::Safe,
            Some("exclusive") => crate::tools::ConcurrencyClass::Exclusive,
            Some(_) => crate::tools::ConcurrencyClass::Exclusive,
        }
    }

    /// A stable, human-readable name for this server (for logs / keyring keys).
    pub fn display_name(&self) -> &str {
        if let Some(name) = &self.name {
            name
        } else if let Some(cmd) = &self.command {
            cmd
        } else if let Some(url) = &self.url {
            url
        } else {
            "unknown"
        }
    }
}

/// Build the octos client identity sent in the MCP `initialize` request.
/// (`ClientInfo`/`Implementation` are `#[non_exhaustive]`, so they can't be
/// built with a struct literal — hence default-then-assign.)
#[allow(clippy::field_reassign_with_default)]
fn octos_client_info() -> ClientInfo {
    let mut info = ClientInfo::default();
    info.client_info = Implementation::new("octos", env!("CARGO_PKG_VERSION"));
    info
}

/// A reqwest DNS resolver that rejects any host resolving to a private,
/// loopback, link-local, or otherwise-blocked address. Applied to EVERY request
/// the client makes — the MCP transport *and* (for OAuth) every metadata /
/// registration / token / refresh endpoint the server advertises — so a public
/// MCP server can't point us at `127.0.0.1` / `169.254.169.254` / RFC1918. Also
/// re-checks on each resolve, defeating DNS rebinding.
#[derive(Debug)]
struct SsrfDnsResolver;

impl reqwest_rmcp::dns::Resolve for SsrfDnsResolver {
    fn resolve(&self, name: reqwest_rmcp::dns::Name) -> reqwest_rmcp::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0u16))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?
                .collect();
            if addrs.is_empty() {
                return Err(format!("no addresses resolved for '{host}'").into());
            }
            if let Some(bad) = addrs
                .iter()
                .find(|a| crate::tools::ssrf::is_private_ip(&a.ip()))
            {
                return Err(format!(
                    "SSRF blocked: '{host}' resolves to private/blocked address {}",
                    bad.ip()
                )
                .into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest_rmcp::dns::Addrs)
        })
    }
}

/// Reject a configured URL whose host is a literal private/loopback IP or
/// `localhost`. reqwest/hyper skip [`SsrfDnsResolver`] for literal-IP hosts, so
/// this closes the config-level vector the resolver can't see. (Hostname hosts
/// are covered by the resolver at connect time.)
pub(crate) fn reject_private_url_host(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).map_err(|e| eyre::eyre!("invalid MCP url '{url}': {e}"))?;
    let blocked = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => crate::tools::ssrf::is_private_ip(&std::net::IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => crate::tools::ssrf::is_private_ip(&std::net::IpAddr::V6(ip)),
        Some(url::Host::Domain(d)) => crate::tools::ssrf::is_private_host(d),
        None => eyre::bail!("MCP url '{url}' has no host"),
    };
    if blocked {
        eyre::bail!("MCP url '{url}' targets a private/loopback host — refused (SSRF)");
    }
    Ok(())
}

/// Build a reqwest client (rmcp's 0.13 major) for talking to a remote MCP
/// server: SSRF-filtered on every host via [`SsrfDnsResolver`], redirects
/// refused (a 3xx must not smuggle us to a private/metadata host), carrying the
/// configured headers verbatim. Shared by the static-HTTP and OAuth transports
/// (and, for OAuth, reused for the discovery/token client so those requests are
/// SSRF-filtered too).
pub(crate) fn build_ssrf_http_client(
    headers: &HashMap<String, String>,
) -> Result<reqwest_rmcp::Client> {
    let mut builder = reqwest_rmcp::Client::builder()
        .redirect(reqwest_rmcp::redirect::Policy::none())
        .dns_resolver(
            std::sync::Arc::new(SsrfDnsResolver) as std::sync::Arc<dyn reqwest_rmcp::dns::Resolve>
        );
    if !headers.is_empty() {
        let mut hmap = reqwest_rmcp::header::HeaderMap::new();
        for (k, v) in headers {
            let name = reqwest_rmcp::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| eyre::eyre!("invalid MCP header name '{k}': {e}"))?;
            let value = reqwest_rmcp::header::HeaderValue::from_str(v)
                .map_err(|e| eyre::eyre!("invalid MCP header value for '{k}': {e}"))?;
            hmap.insert(name, value);
        }
        builder = builder.default_headers(hmap);
    }
    builder
        .build()
        .map_err(|e| eyre::eyre!("build MCP http client: {e}"))
}

/// Per-OAuth-request timeout so a token/refresh/discovery endpoint that accepts
/// the connection but never answers can't hang login/startup.
const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Validate an OAuth endpoint URL before we hit it: must be `https` (bearer
/// tokens never travel cleartext) and must not resolve to a private/loopback/
/// metadata host. Uses `url::Host` so bracketed IPv6 literals (`[::1]`) are
/// handled, unlike a bare `Uri::host()` string.
fn validate_oauth_uri(uri: &oauth2::http::Uri) -> std::result::Result<(), String> {
    let s = uri.to_string();
    let parsed = url::Url::parse(&s).map_err(|e| format!("invalid OAuth url '{s}': {e}"))?;
    if parsed.scheme() != "https" {
        return Err(format!(
            "OAuth endpoint '{s}' must be https — refusing cleartext bearer/credential exchange"
        ));
    }
    let private = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => crate::tools::ssrf::is_private_ip(&std::net::IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => crate::tools::ssrf::is_private_ip(&std::net::IpAddr::V6(ip)),
        Some(url::Host::Domain(d)) => crate::tools::ssrf::is_private_host(d),
        None => return Err(format!("OAuth endpoint '{s}' has no host")),
    };
    if private {
        return Err(format!(
            "SSRF blocked: OAuth endpoint '{s}' is private/loopback"
        ));
    }
    Ok(())
}

/// An rmcp `OAuthHttpClient` that SSRF- and TLS-validates EVERY OAuth request
/// URL before executing it — metadata discovery / registration / token /
/// refresh — including literal-IP and IPv6 endpoints the DNS resolver skips.
/// Redirects are refused (a followed hop would bypass the per-request check),
/// and each request is timeout-bounded.
pub(crate) struct SsrfOAuthHttpClient {
    client: reqwest_rmcp::Client,
}

impl SsrfOAuthHttpClient {
    pub(crate) fn new() -> Result<Self> {
        let client = reqwest_rmcp::Client::builder()
            .redirect(reqwest_rmcp::redirect::Policy::none())
            .timeout(OAUTH_HTTP_TIMEOUT)
            .dns_resolver(std::sync::Arc::new(SsrfDnsResolver)
                as std::sync::Arc<dyn reqwest_rmcp::dns::Resolve>)
            .build()
            .map_err(|e| eyre::eyre!("build OAuth http client: {e}"))?;
        Ok(Self { client })
    }
}

impl rmcp::transport::auth::OAuthHttpClient for SsrfOAuthHttpClient {
    fn execute(
        &self,
        req: rmcp::transport::auth::OAuthHttpRequest,
    ) -> rmcp::transport::auth::OAuthHttpClientFuture<'_> {
        use rmcp::transport::auth::{OAuthHttpClientError, OAuthHttpRequest};
        let client = self.client.clone();
        Box::pin(async move {
            let OAuthHttpRequest { request, .. } = req;
            // Redirects are refused (Policy::none), so the request URL is the
            // only host we contact — validate it (https + non-private, IPv6-safe).
            validate_oauth_uri(request.uri()).map_err(OAuthHttpClientError::new)?;
            let rq = reqwest_rmcp::Request::try_from(request)
                .map_err(|e| OAuthHttpClientError::new(e.to_string()))?;
            let resp = client
                .execute(rq)
                .await
                .map_err(|e| OAuthHttpClientError::new(e.to_string()))?;
            let mut builder = oauth2::http::Response::builder().status(resp.status());
            for (name, value) in resp.headers() {
                builder = builder.header(name, value);
            }
            let body = resp
                .bytes()
                .await
                .map_err(|e| OAuthHttpClientError::new(e.to_string()))?
                .to_vec();
            builder
                .body(body)
                .map_err(|e| OAuthHttpClientError::new(e.to_string()))
        })
    }
}

/// Validate an MCP-provided input schema for reasonable complexity.
fn validate_schema(schema: &serde_json::Value) -> bool {
    fn depth(v: &serde_json::Value, level: usize) -> usize {
        if level > MAX_SCHEMA_DEPTH {
            return level;
        }
        match v {
            serde_json::Value::Object(map) => map
                .values()
                .map(|child| depth(child, level + 1))
                .max()
                .unwrap_or(level),
            serde_json::Value::Array(arr) => arr
                .iter()
                .map(|child| depth(child, level + 1))
                .max()
                .unwrap_or(level),
            _ => level,
        }
    }
    if depth(schema, 0) > MAX_SCHEMA_DEPTH {
        return false;
    }
    serde_json::to_string(schema)
        .map(|s| s.len())
        .unwrap_or(MAX_SCHEMA_SIZE + 1)
        <= MAX_SCHEMA_SIZE
}

/// Public live status for one configured MCP connector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub id: String,
    pub display_name: String,
    pub transport: String,
    pub enabled: bool,
    pub status: String,
    pub tool_count: usize,
    pub tools: Vec<String>,
    pub error: Option<String>,
    /// T-0538: whether this connector uses OAuth (from its config), so a fresh
    /// page can offer re-authorization on an expired `failed` row. Always
    /// serialized (false included) — the web distinguishes old/new Core by
    /// the field's presence.
    pub oauth: bool,
}

#[derive(Clone)]
struct McpServerRuntimeStatus {
    config: McpServerConfig,
    status: String,
    tools: Vec<String>,
    error: Option<String>,
    service: Option<McpService>,
}

/// A discovered MCP tool bound to its live session.
#[derive(Clone)]
struct McpToolSpec {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    service: McpService,
    concurrency_class: crate::tools::ConcurrencyClass,
}

/// A running set of MCP server connections and the tools they expose.
#[derive(Clone)]
pub struct McpClient {
    /// The live transports (and their stdio child processes).
    ///
    /// Cloned into the registry by [`McpClient::register_tools`], which owns
    /// them for its lifetime. The registry must retain them even when every
    /// tool is filtered out; otherwise profile narrowing tears down the
    /// connection (#1886). The ProfileRuntime also keeps an Arc<McpClient> so
    /// status reads observe the same live transports.
    services: Vec<(String, McpService)>,
    tools: Vec<McpToolSpec>,
    statuses: Vec<McpServerRuntimeStatus>,
}

impl McpClient {
    /// Built-in tool names that MCP tools must not shadow.
    const PROTECTED_NAMES: &[&str] = &[
        "shell",
        "read_file",
        "discover_tools",
        "run_tool",
        "write_file",
        "edit_file",
        "diff_edit",
        "glob",
        "grep",
        "list_dir",
        "web_search",
        "web_fetch",
        "browser",
        "git",
        "message",
        "send_file",
        "spawn",
        "voice_synthesize",
        "save_memory",
        "recall_memory",
        "record_memory_use",
        "configure_tool",
        // The rest of the compiled-in tools an external client of
        // `octos serve --host-managed` keeps (UPCR-2026-036), and the
        // delegation aliases.
        "apply_patch",
        "code_structure",
        "check_workspace_contract",
        "ask_user_question",
        "recall",
        "memory_search",
        "memory_load",
        "view_image",
        "view_video",
        "tool_search",
        "spawn_agent",
        "delegate",
    ];

    /// Start all configured MCP servers and discover their tools. Fail-soft: a
    /// server that fails to start is logged and skipped, never aborting the rest.
    pub async fn start(configs: &[McpServerConfig]) -> Result<Self> {
        Self::start_scoped(configs, None, None).await
    }

    pub async fn start_scoped(
        configs: &[McpServerConfig],
        profile_id: Option<&str>,
        data_dir: Option<&std::path::Path>,
    ) -> Result<Self> {
        let mut services = Vec::new();
        let mut tools = Vec::new();
        let mut statuses = Vec::new();

        for config in configs {
            let server_name = config.display_name().to_string();
            if !config.enabled {
                statuses.push(McpServerRuntimeStatus {
                    config: config.clone(),
                    status: "disabled".into(),
                    tools: Vec::new(),
                    error: None,
                    service: None,
                });
                continue;
            }
            match Self::connect_scoped(config, profile_id, data_dir).await {
                Ok(service) => {
                    let concurrency_class = config.resolved_concurrency_class();
                    // Bound tool discovery: a server that completes `initialize`
                    // but never answers `tools/list` must not wedge startup (and
                    // block later servers) — fail-soft on timeout.
                    let discovered = match timeout(HANDSHAKE_TIMEOUT, service.list_all_tools())
                        .await
                    {
                        Ok(Ok(t)) => t,
                        Ok(Err(e)) => {
                            warn!(server = server_name, error = %e, "MCP tools/list failed, skipping server");
                            statuses.push(McpServerRuntimeStatus {
                                config: config.clone(),
                                status: "failed".into(),
                                tools: Vec::new(),
                                error: Some(e.to_string()),
                                service: Some(service.clone()),
                            });
                            continue;
                        }
                        Err(_) => {
                            warn!(
                                server = server_name,
                                "MCP tools/list timed out, skipping server"
                            );
                            statuses.push(McpServerRuntimeStatus {
                                config: config.clone(),
                                status: "failed".into(),
                                tools: Vec::new(),
                                error: Some("MCP tools/list timed out".into()),
                                service: Some(service.clone()),
                            });
                            continue;
                        }
                    };
                    statuses.push(McpServerRuntimeStatus {
                        config: config.clone(),
                        status: "connected".into(),
                        tools: discovered
                            .iter()
                            .map(|tool| tool.name.to_string())
                            .collect(),
                        error: None,
                        service: Some(service.clone()),
                    });
                    info!(
                        server = server_name,
                        tools = discovered.len(),
                        concurrency_class = ?concurrency_class,
                        "MCP server started"
                    );
                    for tool in discovered {
                        let schema = serde_json::Value::Object((*tool.input_schema).clone());
                        if !validate_schema(&schema) {
                            warn!(
                                server = server_name,
                                tool = %tool.name,
                                "MCP tool schema exceeds depth/size limits, skipping"
                            );
                            continue;
                        }
                        tools.push(McpToolSpec {
                            name: tool.name.to_string(),
                            description: tool
                                .description
                                .map(|d| d.to_string())
                                .unwrap_or_default(),
                            input_schema: schema,
                            service: service.clone(),
                            concurrency_class,
                        });
                    }
                    services.push((server_name, service));
                }
                Err(e) => {
                    warn!(server = server_name, error = %e, "failed to start MCP server, skipping");
                    statuses.push(McpServerRuntimeStatus {
                        config: config.clone(),
                        status: "failed".into(),
                        tools: Vec::new(),
                        error: Some(e.to_string()),
                        service: None,
                    });
                }
            }
        }

        Ok(Self {
            services,
            tools,
            statuses,
        })
    }

    pub fn statuses(&self) -> Vec<McpServerStatus> {
        self.statuses
            .iter()
            .map(|entry| {
                let status = if entry
                    .service
                    .as_ref()
                    .is_some_and(|service| service.is_closed())
                {
                    "closed"
                } else {
                    entry.status.as_str()
                };
                McpServerStatus {
                    id: entry.config.connector_id(),
                    display_name: entry.config.display_name().to_string(),
                    transport: entry.config.transport_name().to_string(),
                    enabled: entry.config.enabled,
                    status: status.to_string(),
                    tool_count: entry.tools.len(),
                    tools: entry.tools.clone(),
                    error: entry.error.clone(),
                    oauth: entry.config.oauth,
                }
            })
            .collect()
    }

    /// Connect to one MCP server, returning the live rmcp session.
    async fn connect_scoped(
        config: &McpServerConfig,
        profile_id: Option<&str>,
        data_dir: Option<&std::path::Path>,
    ) -> Result<McpService> {
        if config.url.is_some() {
            Self::connect_http(config, profile_id, data_dir).await
        } else {
            Self::connect_stdio(config).await
        }
    }

    /// Spawn a stdio MCP server as a child process. Environment is sanitized
    /// (BLOCKED_ENV_VARS stripped, only explicitly-configured names forwarded)
    /// exactly as for every other octos subprocess.
    async fn connect_stdio(config: &McpServerConfig) -> Result<McpService> {
        let command = config
            .command
            .as_deref()
            .ok_or_else(|| eyre::eyre!("MCP stdio server requires a 'command' field"))?;

        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&config.args).kill_on_drop(true);

        // Strip injection-vector env vars from the inherited environment and
        // forward only the names the operator explicitly listed under `env`.
        let allowlist = EnvAllowlist::from_names(config.env.keys().map(|k| k.as_str()));
        sanitize_command_env(&mut cmd, &allowlist);
        for (k, v) in &config.env {
            // Re-apply the same denylist to the *explicit* env: a config that
            // lists e.g. LD_PRELOAD/DYLD_INSERT_LIBRARIES/NODE_OPTIONS must not
            // reopen a process-hijack vector that sanitize_command_env stripped.
            if !should_forward_env_name(k, &allowlist) {
                warn!(command = command, var = %k, "MCP env var blocked (injection vector), not forwarded");
                continue;
            }
            cmd.env(k, v);
        }

        // R-12: rmcp's TokioChildProcess transport reads JSON-RPC frames with
        // an unbounded `read_until` and cannot accept a custom reader, so we
        // spawn the child ourselves and hand `serve_client` a (bounded stdout,
        // stdin) pair — the cap lives in `BoundedChildStdout`. The wrapper
        // owns the child so `kill_on_drop` still terminates the server when
        // the transport is dropped.
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .wrap_err_with(|| format!("failed to spawn MCP server '{command}'"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| eyre::eyre!("MCP server '{command}' stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| eyre::eyre!("MCP server '{command}' stdout was not piped"))?;
        let bounded_stdout = BoundedChildStdout {
            _child: child,
            inner: BoundedLineRead::new(stdout),
        };

        let service = timeout(
            HANDSHAKE_TIMEOUT,
            serve_client(octos_client_info(), (bounded_stdout, stdin)),
        )
        .await
        .map_err(|_| eyre::eyre!("MCP handshake timed out after {HANDSHAKE_TIMEOUT:?}"))?
        .map_err(|e| eyre::eyre!("MCP initialize failed: {e}"))?;
        Ok(Arc::new(service))
    }

    /// Connect to a streamable-HTTP MCP server. Static headers (incl. a bearer
    /// `Authorization`) are sent verbatim; OAuth 2.1 via keyring-stored tokens.
    async fn connect_http(
        config: &McpServerConfig,
        profile_id: Option<&str>,
        data_dir: Option<&std::path::Path>,
    ) -> Result<McpService> {
        let url = config
            .url
            .as_deref()
            .ok_or_else(|| eyre::eyre!("MCP http server requires a 'url' field"))?;

        if config.oauth {
            return match (profile_id, data_dir) {
                (Some(profile_id), Some(data_dir)) => {
                    crate::mcp_auth::connect_oauth_scoped(
                        config,
                        url,
                        octos_client_info(),
                        profile_id,
                        data_dir,
                    )
                    .await
                }
                _ => crate::mcp_auth::connect_oauth(config, url, octos_client_info()).await,
            };
        }

        // SSRF-filtered + no-redirect client carrying the configured headers
        // verbatim (no double `Bearer`, custom headers kept). Reject literal
        // private-IP hosts up front (the resolver is skipped for those).
        reject_private_url_host(url)?;
        let client = build_ssrf_http_client(&config.headers)?;
        let transport = StreamableHttpClientTransport::with_client(
            client,
            StreamableHttpClientTransportConfig::with_uri(url.to_string()),
        );

        let service = timeout(
            HANDSHAKE_TIMEOUT,
            serve_client(octos_client_info(), transport),
        )
        .await
        .map_err(|_| eyre::eyre!("MCP handshake timed out after {HANDSHAKE_TIMEOUT:?}"))?
        .map_err(|e| eyre::eyre!("MCP initialize failed: {e}"))?;
        Ok(Arc::new(service))
    }

    /// Register all discovered MCP tools into the given registry. Tools whose
    /// names collide with built-in tool names are rejected so a remote server
    /// cannot silently replace core functionality.
    pub fn register_tools(&self, registry: &mut ToolRegistry) {
        // Hand the transports to the registry BEFORE the tools, so it owns them
        // even if every tool below is later filtered out. Without this the tools
        // are the sole owners and profile narrowing tears down the connection
        // (#1886) — a visibility filter must not end a child process.
        for (_server, service) in self.services.clone() {
            registry.keep_mcp_service_alive(service as Arc<dyn std::any::Any + Send + Sync>);
        }
        for spec in self.tools.clone() {
            if Self::PROTECTED_NAMES.contains(&spec.name.as_str())
                || registry.is_builtin_name(&spec.name)
            {
                warn!(
                    tool = spec.name,
                    "MCP tool name collides with built-in tool, skipping"
                );
                continue;
            }
            registry.register_with_origin(
                McpTool {
                    name: spec.name,
                    description: spec.description,
                    input_schema: spec.input_schema,
                    service: spec.service,
                    concurrency_class: spec.concurrency_class,
                    workspace: registry.workspace_root().map(std::path::Path::to_path_buf),
                },
                crate::tools::ToolOrigin::Mcp,
            );
        }
    }
}

/// A tool backed by an MCP server session.
struct McpTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    service: McpService,
    concurrency_class: crate::tools::ConcurrencyClass,
    workspace: Option<std::path::PathBuf>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn concurrency_class(&self) -> crate::tools::ConcurrencyClass {
        self.concurrency_class
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        self.execute_with_context(&crate::tools::ToolContext::zero(), args)
            .await
    }

    async fn execute_with_context(
        &self,
        ctx: &crate::tools::ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let mut param = CallToolRequestParams::new(self.name.clone());
        param.arguments = args.as_object().cloned();

        // T-0241: send the request via the cancellable handle so the request
        // id is known — the turn-cancel arm below can then tell the server
        // exactly which call to abandon (`notifications/cancelled`), instead
        // of only dropping the local future while the remote work continues.
        let request =
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(param));
        let handle = self
            .service
            .send_cancellable_request(request, rmcp::service::PeerRequestOptions::no_options())
            .await
            .map_err(|e| eyre::eyre!("MCP tool '{}' call failed: {e}", self.name))?;
        let request_id = handle.id.clone();
        let timed = timeout(TOOL_CALL_TIMEOUT, handle.await_response());
        let result = match ctx.turn_cancel.clone() {
            Some(token) => {
                tokio::pin!(timed);
                tokio::select! {
                    r = &mut timed => Some(r),
                    _ = token.cancelled() => {
                        // Best-effort per the MCP spec: the notification rides
                        // the same transport; a dead transport fails silently.
                        // Dropping the response future ends the local wait.
                        let _ = self
                            .service
                            .notify_cancelled(rmcp::model::CancelledNotificationParam::new(
                                Some(request_id),
                                Some("turn interrupted".into()),
                            ))
                            .await;
                        None
                    }
                }
            }
            None => Some(timed.await),
        };
        let Some(result) = result else {
            return Ok(ToolResult {
                output: format!(
                    "MCP tool '{}' call cancelled: the turn was interrupted (Stop / turn/interrupt / work-secret revocation). The server was sent notifications/cancelled for this request; a server that ignores it may still finish the call remotely — verify before assuming the operation did not happen.",
                    self.name
                ),
                success: false,
                ..Default::default()
            });
        };

        let result = result
            .map_err(|_| {
                eyre::eyre!(
                    "MCP tool '{}' call timed out after {TOOL_CALL_TIMEOUT:?}",
                    self.name
                )
            })?
            .map_err(|e| eyre::eyre!("MCP tool '{}' call failed: {e}", self.name))?;
        let result = match result {
            rmcp::model::ServerResult::CallToolResult(result) => result,
            _ => {
                return Err(eyre::eyre!(
                    "MCP tool '{}' returned an unexpected response shape",
                    self.name
                ));
            }
        };

        let current = std::env::current_dir()?;
        let workspace = ctx
            .session_scope
            .as_ref()
            .map(|s| s.workspace())
            .or(self.workspace.as_deref())
            .unwrap_or(&current);
        Ok(content::render(result, workspace))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_schema_rejects_too_deep_and_too_big() {
        assert!(validate_schema(&serde_json::json!({"type": "object"})));
        // Build a schema deeper than MAX_SCHEMA_DEPTH.
        let mut v = serde_json::json!("leaf");
        for _ in 0..(MAX_SCHEMA_DEPTH + 3) {
            v = serde_json::json!({ "nested": v });
        }
        assert!(!validate_schema(&v));
        // Oversized flat schema.
        let big: String = "x".repeat(MAX_SCHEMA_SIZE + 10);
        assert!(!validate_schema(&serde_json::json!({ "d": big })));
    }

    fn cfg() -> McpServerConfig {
        McpServerConfig {
            id: None,
            name: None,
            enabled: true,
            allowlist_entry_id: None,
            command: Some("echo".into()),
            args: vec![],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            oauth: false,
            scopes: vec![],
            concurrency_class: None,
        }
    }

    /// T-0538: the runtime statuses() path must surface the config's oauth
    /// flag on every row — true for an OAuth connector, false (present, not
    /// omitted) for a plain one — so a fresh page can offer re-authorization
    /// without guessing.
    #[tokio::test]
    async fn statuses_surface_config_oauth_flag_for_both_kinds() {
        // Distinct ids; disabled so start_scoped never connects (statuses()
        // still emits a row per config).
        let mut oauth_cfg = cfg();
        oauth_cfg.id = Some("gh".into());
        oauth_cfg.oauth = true;
        oauth_cfg.enabled = false;
        let mut plain_cfg = cfg();
        plain_cfg.id = Some("fs".into());
        plain_cfg.enabled = false;
        let client = McpClient::start(&[oauth_cfg, plain_cfg])
            .await
            .expect("start");
        let statuses = client.statuses();
        assert_eq!(statuses.len(), 2, "both configs produce a status row");
        let oauth_row = statuses.iter().find(|s| s.id == "gh").expect("oauth row");
        assert!(oauth_row.oauth, "oauth config surfaces oauth: true");
        let plain_row = statuses.iter().find(|s| s.id == "fs").expect("plain row");
        assert!(!plain_row.oauth, "plain config surfaces oauth: false");
    }

    #[test]
    fn protected_names_cover_core_builtins() {
        for name in [
            "shell",
            "read_file",
            "write_file",
            "edit_file",
            "send_file",
            "spawn",
        ] {
            assert!(
                McpClient::PROTECTED_NAMES.contains(&name),
                "{name} must be protected"
            );
        }
    }
}
