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
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use rmcp::model::{CallToolRequestParams, ClientInfo, Implementation};
use rmcp::service::{RoleClient, RunningService, serve_client};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tracing::{info, warn};

use crate::subprocess_env::{EnvAllowlist, sanitize_command_env, should_forward_env_name};
use crate::tools::{Tool, ToolRegistry, ToolResult};

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

impl McpServerConfig {
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
        if let Some(cmd) = &self.command {
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
            if let Some(bad) = addrs.iter().find(|a| crate::tools::ssrf::is_private_ip(&a.ip())) {
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
        .dns_resolver(std::sync::Arc::new(SsrfDnsResolver) as std::sync::Arc<dyn reqwest_rmcp::dns::Resolve>);
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

/// A discovered MCP tool bound to its live session.
struct McpToolSpec {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    service: McpService,
    concurrency_class: crate::tools::ConcurrencyClass,
}

/// A running set of MCP server connections and the tools they expose.
pub struct McpClient {
    /// Kept alive so the underlying transports (and stdio child processes) stay
    /// open for as long as any registered tool references them.
    #[allow(dead_code)]
    services: Vec<(String, McpService)>,
    tools: Vec<McpToolSpec>,
}

impl McpClient {
    /// Built-in tool names that MCP tools must not shadow.
    const PROTECTED_NAMES: &[&str] = &[
        "shell",
        "read_file",
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
        "configure_tool",
    ];

    /// Start all configured MCP servers and discover their tools. Fail-soft: a
    /// server that fails to start is logged and skipped, never aborting the rest.
    pub async fn start(configs: &[McpServerConfig]) -> Result<Self> {
        let mut services = Vec::new();
        let mut tools = Vec::new();

        for config in configs {
            let server_name = config.display_name().to_string();
            match Self::connect(config).await {
                Ok(service) => {
                    let concurrency_class = config.resolved_concurrency_class();
                    // Bound tool discovery: a server that completes `initialize`
                    // but never answers `tools/list` must not wedge startup (and
                    // block later servers) — fail-soft on timeout.
                    let discovered = match timeout(HANDSHAKE_TIMEOUT, service.list_all_tools()).await
                    {
                        Ok(Ok(t)) => t,
                        Ok(Err(e)) => {
                            warn!(server = server_name, error = %e, "MCP tools/list failed, skipping server");
                            continue;
                        }
                        Err(_) => {
                            warn!(server = server_name, "MCP tools/list timed out, skipping server");
                            continue;
                        }
                    };
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
                }
            }
        }

        Ok(Self { services, tools })
    }

    /// Connect to one MCP server, returning the live rmcp session.
    async fn connect(config: &McpServerConfig) -> Result<McpService> {
        if config.url.is_some() {
            Self::connect_http(config).await
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

        // NOTE: rmcp's child-process transport reads JSON-RPC frames with an
        // unbounded `read_until`, so it lacks the old client's MAX_LINE_BYTES
        // guard. A malicious frame could grow memory before schema validation.
        // Accepted for now: stdio servers are operator-configured local binaries
        // (the operator already trusts the executable they named). A bounded
        // frame codec would require a custom transport; tracked as a follow-up.
        let (transport, _stderr) = TokioChildProcess::builder(cmd)
            .stderr(Stdio::inherit())
            .spawn()
            .wrap_err_with(|| format!("failed to spawn MCP server '{command}'"))?;

        let service = timeout(HANDSHAKE_TIMEOUT, serve_client(octos_client_info(), transport))
            .await
            .map_err(|_| eyre::eyre!("MCP handshake timed out after {HANDSHAKE_TIMEOUT:?}"))?
            .map_err(|e| eyre::eyre!("MCP initialize failed: {e}"))?;
        Ok(Arc::new(service))
    }

    /// Connect to a streamable-HTTP MCP server. Static headers (incl. a bearer
    /// `Authorization`) are sent verbatim; OAuth 2.1 via keyring-stored tokens.
    async fn connect_http(config: &McpServerConfig) -> Result<McpService> {
        let url = config
            .url
            .as_deref()
            .ok_or_else(|| eyre::eyre!("MCP http server requires a 'url' field"))?;

        if config.oauth {
            return crate::mcp_auth::connect_oauth(config, url, octos_client_info()).await;
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

        let service = timeout(HANDSHAKE_TIMEOUT, serve_client(octos_client_info(), transport))
            .await
            .map_err(|_| eyre::eyre!("MCP handshake timed out after {HANDSHAKE_TIMEOUT:?}"))?
            .map_err(|e| eyre::eyre!("MCP initialize failed: {e}"))?;
        Ok(Arc::new(service))
    }

    /// Register all discovered MCP tools into the given registry. Tools whose
    /// names collide with built-in tool names are rejected so a remote server
    /// cannot silently replace core functionality.
    pub fn register_tools(self, registry: &mut ToolRegistry) {
        for spec in self.tools {
            if Self::PROTECTED_NAMES.contains(&spec.name.as_str()) {
                warn!(tool = spec.name, "MCP tool name collides with built-in tool, skipping");
                continue;
            }
            registry.register(McpTool {
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
                service: spec.service,
                concurrency_class: spec.concurrency_class,
            });
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
        let mut param = CallToolRequestParams::new(self.name.clone());
        param.arguments = args.as_object().cloned();

        let result = timeout(TOOL_CALL_TIMEOUT, self.service.call_tool(param))
            .await
            .map_err(|_| eyre::eyre!("MCP tool '{}' call timed out after {TOOL_CALL_TIMEOUT:?}", self.name))?
            .map_err(|e| eyre::eyre!("MCP tool '{}' call failed: {e}", self.name))?;

        // Flatten text content parts; non-text parts (images/resources) are
        // dropped as the agent tool surface is text-only.
        let output = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolResult {
            output,
            success: !result.is_error.unwrap_or(false),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ConcurrencyClass;

    fn cfg() -> McpServerConfig {
        McpServerConfig {
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

    #[test]
    fn concurrency_class_defaults_to_safe_and_fails_safe_on_typo() {
        let mut c = cfg();
        assert_eq!(c.resolved_concurrency_class(), ConcurrencyClass::Safe);
        c.concurrency_class = Some("SAFE".into());
        assert_eq!(c.resolved_concurrency_class(), ConcurrencyClass::Safe);
        c.concurrency_class = Some("Exclusive".into());
        assert_eq!(c.resolved_concurrency_class(), ConcurrencyClass::Exclusive);
        c.concurrency_class = Some("bogus".into());
        assert_eq!(c.resolved_concurrency_class(), ConcurrencyClass::Exclusive);
    }

    #[test]
    fn display_name_prefers_command_then_url() {
        let mut c = cfg();
        assert_eq!(c.display_name(), "echo");
        c.command = None;
        c.url = Some("https://example.com/mcp".into());
        assert_eq!(c.display_name(), "https://example.com/mcp");
        c.url = None;
        assert_eq!(c.display_name(), "unknown");
    }

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

    #[test]
    fn protected_names_cover_core_builtins() {
        for name in ["shell", "read_file", "write_file", "edit_file", "send_file", "spawn"] {
            assert!(
                McpClient::PROTECTED_NAMES.contains(&name),
                "{name} must be protected"
            );
        }
    }

    #[test]
    fn oauth_and_scopes_default_off() {
        let c = cfg();
        assert!(!c.oauth);
        assert!(c.scopes.is_empty());
    }

    #[test]
    fn reject_private_url_host_blocks_private_allows_public() {
        // Literal private/loopback/metadata hosts (resolver is skipped for these).
        assert!(reject_private_url_host("http://127.0.0.1/mcp").is_err());
        assert!(reject_private_url_host("http://localhost:8000/mcp").is_err());
        assert!(reject_private_url_host("http://169.254.169.254/latest").is_err());
        assert!(reject_private_url_host("http://[::1]/mcp").is_err());
        assert!(reject_private_url_host("http://10.0.0.5/mcp").is_err());
        // Public hostnames pass the up-front check (resolver enforces at connect).
        assert!(reject_private_url_host("https://example.com/mcp").is_ok());
    }
}
