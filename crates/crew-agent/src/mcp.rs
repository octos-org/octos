//! MCP (Model Context Protocol) client for external tool integration.
//!
//! Supports two transport modes:
//! - **stdio**: Spawns MCP servers as child processes, communicates via stdin/stdout JSON-RPC.
//! - **HTTP**: Connects to remote MCP servers via HTTP POST for JSON-RPC requests.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::tools::{Tool, ToolRegistry, ToolResult};

/// Maximum size for a single JSON-RPC response line (1MB).
const MAX_LINE_BYTES: usize = 1_048_576;

use crate::sandbox::BLOCKED_ENV_VARS;

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
    /// HTTP transport: additional headers (e.g. Authorization).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

impl McpServerConfig {
    fn display_name(&self) -> &str {
        if let Some(cmd) = &self.command {
            cmd
        } else if let Some(url) = &self.url {
            url
        } else {
            "unknown"
        }
    }
}

/// A running MCP server connection (stdio or HTTP).
enum McpConnection {
    Stdio(StdioMcpConnection),
    Http(HttpMcpConnection),
}

impl McpConnection {
    async fn rpc_call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match self {
            McpConnection::Stdio(c) => c.rpc_call(method, params).await,
            McpConnection::Http(c) => c.rpc_call(method, params).await,
        }
    }
}

/// Stdio-based MCP connection (child process).
struct StdioMcpConnection {
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    child: tokio::process::Child,
    next_id: u64,
}

impl StdioMcpConnection {
    async fn rpc_call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;

        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };

        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;

        let buf = read_line_limited(&mut self.reader, MAX_LINE_BYTES).await?;

        let response: JsonRpcResponse =
            serde_json::from_str(&buf).wrap_err("invalid JSON-RPC response from MCP server")?;

        if let Some(err) = response.error {
            eyre::bail!("MCP error {}: {}", err.code, err.message);
        }

        response
            .result
            .ok_or_else(|| eyre::eyre!("MCP response missing result"))
    }
}

/// Read a single line with a size limit to prevent memory exhaustion.
async fn read_line_limited(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    limit: usize,
) -> Result<String> {
    let mut buf = Vec::with_capacity(4096);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            eyre::bail!("MCP server closed connection");
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=pos]);
            reader.consume(pos + 1);
            break;
        }
        // Check BEFORE extending to enforce strict limit
        if buf.len() + available.len() > limit {
            eyre::bail!("MCP response exceeds {}KB limit", limit / 1024);
        }
        let len = available.len();
        buf.extend_from_slice(available);
        reader.consume(len);
    }
    String::from_utf8(buf).wrap_err("MCP response is not valid UTF-8")
}

impl Drop for StdioMcpConnection {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        // Reap child to avoid zombie processes
        let _ = self.child.try_wait();
    }
}

/// HTTP-based MCP connection (remote server).
struct HttpMcpConnection {
    client: reqwest::Client,
    url: String,
    headers: HashMap<String, String>,
    next_id: u64,
    /// Session ID returned by the server for request affinity.
    session_id: Option<String>,
}

impl HttpMcpConnection {
    async fn rpc_call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;

        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };

        let mut req = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid.as_str());
        }

        let resp = req
            .json(&request)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .wrap_err("failed to send HTTP request to MCP server")?;

        // Capture session ID from response if present.
        if let Some(sid) = resp.headers().get("mcp-session-id") {
            if let Ok(s) = sid.to_str() {
                self.session_id = Some(s.to_string());
            }
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("MCP HTTP error: {status} - {body}");
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = resp
            .text()
            .await
            .wrap_err("failed to read MCP HTTP response")?;

        // SSE responses: extract JSON-RPC message from data events.
        let json_text = if content_type.contains("text/event-stream") {
            parse_sse_json_rpc(&body)?
        } else {
            body
        };

        let response: JsonRpcResponse = serde_json::from_str(&json_text)
            .wrap_err("invalid JSON-RPC response from MCP HTTP server")?;

        if let Some(err) = response.error {
            eyre::bail!("MCP error {}: {}", err.code, err.message);
        }

        response
            .result
            .ok_or_else(|| eyre::eyre!("MCP response missing result"))
    }
}

/// Extract the last JSON-RPC data payload from an SSE response body.
fn parse_sse_json_rpc(body: &str) -> Result<String> {
    let mut last_data = None;
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data != "[DONE]" {
                last_data = Some(data.to_string());
            }
        }
    }
    last_data.ok_or_else(|| eyre::eyre!("no JSON-RPC data in SSE response"))
}

/// MCP client that manages server connections and tool registration.
pub struct McpClient {
    /// Kept alive so Drop kills child processes (stdio) / holds HTTP clients.
    #[allow(dead_code)]
    connections: Vec<(String, Arc<Mutex<McpConnection>>)>,
    tools: Vec<McpToolSpec>,
}

struct McpToolSpec {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    connection: Arc<Mutex<McpConnection>>,
}

/// Maximum nesting depth for MCP tool input schemas.
const MAX_SCHEMA_DEPTH: usize = 10;
/// Maximum serialized size of an MCP tool input schema (64 KB).
const MAX_SCHEMA_SIZE: usize = 65_536;

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
    let d = depth(schema, 0);
    if d > MAX_SCHEMA_DEPTH {
        return false;
    }
    let size = serde_json::to_string(schema)
        .map(|s| s.len())
        .unwrap_or(MAX_SCHEMA_SIZE + 1);
    size <= MAX_SCHEMA_SIZE
}

impl McpClient {
    /// Start all configured MCP servers and discover their tools.
    pub async fn start(configs: &[McpServerConfig]) -> Result<Self> {
        let mut connections = Vec::new();
        let mut tools = Vec::new();

        for config in configs {
            let result = if config.url.is_some() {
                Self::start_http_server(config).await
            } else {
                Self::start_stdio_server(config).await
            };

            match result {
                Ok((conn, server_tools)) => {
                    let server_name = config.display_name().to_string();
                    let conn = Arc::new(Mutex::new(conn));
                    info!(
                        server = server_name,
                        tools = server_tools.len(),
                        "MCP server started"
                    );
                    for tool in server_tools {
                        let schema = tool
                            .input_schema
                            .unwrap_or(serde_json::json!({"type": "object"}));
                        if !validate_schema(&schema) {
                            warn!(
                                server = server_name,
                                tool = tool.name,
                                "MCP tool schema exceeds depth/size limits, skipping"
                            );
                            continue;
                        }
                        tools.push(McpToolSpec {
                            name: tool.name,
                            description: tool.description.unwrap_or_default(),
                            input_schema: schema,
                            connection: conn.clone(),
                        });
                    }
                    connections.push((server_name, conn));
                }
                Err(e) => {
                    warn!(
                        server = config.display_name(),
                        error = %e,
                        "failed to start MCP server, skipping"
                    );
                }
            }
        }

        Ok(Self { connections, tools })
    }

    async fn start_stdio_server(
        config: &McpServerConfig,
    ) -> Result<(McpConnection, Vec<McpToolDef>)> {
        let command = config
            .command
            .as_deref()
            .ok_or_else(|| eyre::eyre!("MCP stdio server requires 'command' field"))?;

        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()); // Forward stderr for debugging

        for (k, v) in &config.env {
            if BLOCKED_ENV_VARS
                .iter()
                .any(|blocked| k.eq_ignore_ascii_case(blocked))
            {
                warn!(
                    key = k,
                    "blocked dangerous MCP environment variable, skipping"
                );
                continue;
            }
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .wrap_err_with(|| format!("failed to spawn MCP server: {command}"))?;

        let stdin = child.stdin.take().ok_or_else(|| eyre::eyre!("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| eyre::eyre!("no stdout"))?;
        let reader = BufReader::new(stdout);

        let conn = McpConnection::Stdio(StdioMcpConnection {
            stdin,
            reader,
            child,
            next_id: 1,
        });

        initialize_and_list_tools(conn).await
    }

    async fn start_http_server(
        config: &McpServerConfig,
    ) -> Result<(McpConnection, Vec<McpToolDef>)> {
        let url = config
            .url
            .as_deref()
            .ok_or_else(|| eyre::eyre!("MCP HTTP server requires 'url' field"))?;

        // Validate URL against SSRF before connecting to prevent reaching
        // internal endpoints through MCP config.
        if let Some(msg) = crate::tools::ssrf::check_ssrf(url).await {
            eyre::bail!("MCP HTTP server URL blocked by SSRF policy: {msg}");
        }

        let conn = McpConnection::Http(HttpMcpConnection {
            client: reqwest::Client::new(),
            url: url.to_string(),
            headers: config.headers.clone(),
            next_id: 1,
            session_id: None,
        });

        initialize_and_list_tools(conn).await
    }

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

    /// Register all discovered MCP tools into the given registry.
    ///
    /// Tools whose names collide with built-in tool names are rejected to prevent
    /// a remote MCP server from silently replacing core functionality.
    pub fn register_tools(self, registry: &mut ToolRegistry) {
        for spec in self.tools {
            if Self::PROTECTED_NAMES.contains(&spec.name.as_str()) {
                warn!(
                    tool = spec.name,
                    "MCP tool name collides with built-in tool, skipping"
                );
                continue;
            }
            registry.register(McpTool {
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
                connection: spec.connection,
            });
        }
    }
}

/// Shared initialization sequence for both transports.
async fn initialize_and_list_tools(
    mut conn: McpConnection,
) -> Result<(McpConnection, Vec<McpToolDef>)> {
    conn.rpc_call(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "crew-rs", "version": env!("CARGO_PKG_VERSION")}
        }),
    )
    .await
    .wrap_err("MCP initialize failed")?;

    let tools_result = conn
        .rpc_call("tools/list", serde_json::json!({}))
        .await
        .wrap_err("MCP tools/list failed")?;

    let tool_list: McpToolListResponse =
        serde_json::from_value(tools_result).wrap_err("failed to parse MCP tools/list response")?;

    Ok((conn, tool_list.tools))
}

/// A tool backed by an MCP server.
struct McpTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    connection: Arc<Mutex<McpConnection>>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let result = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mut conn = self.connection.lock().await;
            conn.rpc_call(
                "tools/call",
                serde_json::json!({
                    "name": self.name,
                    "arguments": args,
                }),
            )
            .await
        })
        .await
        .wrap_err("MCP tool call timed out after 60s")?
        .wrap_err_with(|| format!("MCP tool '{}' call failed", self.name))?;

        // Parse MCP tool result: { "content": [{"type": "text", "text": "..."}] }
        let output = if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            content
                .iter()
                .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            result.to_string()
        };

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(ToolResult {
            output,
            success: !is_error,
            ..Default::default()
        })
    }
}

// --- JSON-RPC types ---

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: serde_json::Value,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    id: u64,
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct McpToolListResponse {
    tools: Vec<McpToolDef>,
}

#[derive(Deserialize)]
struct McpToolDef {
    name: String,
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- SSE parsing ---

    #[test]
    fn test_parse_sse_json_rpc() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let result = parse_sse_json_rpc(body).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["id"], 1);
    }

    #[test]
    fn test_parse_sse_skips_done() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\ndata: [DONE]\n\n";
        let result = parse_sse_json_rpc(body).unwrap();
        assert!(result.contains("\"id\":1"));
    }

    #[test]
    fn test_parse_sse_empty_body() {
        let body = "event: ping\n\n";
        assert!(parse_sse_json_rpc(body).is_err());
    }

    #[test]
    fn test_parse_sse_returns_last_data_line() {
        let body = "data: {\"id\":1}\ndata: {\"id\":2}\n\n";
        let result = parse_sse_json_rpc(body).unwrap();
        assert!(result.contains("\"id\":2"));
    }

    #[test]
    fn test_parse_sse_no_data_prefix() {
        let body = "id: 1\nevent: open\n\n";
        assert!(parse_sse_json_rpc(body).is_err());
    }

    // --- Config deserialization ---

    #[test]
    fn test_config_deser_stdio() {
        let json = r#"{"command": "npx", "args": ["-y", "mcp-server"]}"#;
        let config: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.command.as_deref(), Some("npx"));
        assert!(config.url.is_none());
        assert_eq!(config.display_name(), "npx");
    }

    #[test]
    fn test_config_deser_http() {
        let json =
            r#"{"url": "https://mcp.example.com/sse", "headers": {"Authorization": "Bearer tok"}}"#;
        let config: McpServerConfig = serde_json::from_str(json).unwrap();
        assert!(config.command.is_none());
        assert_eq!(config.url.as_deref(), Some("https://mcp.example.com/sse"));
        assert_eq!(config.headers.get("Authorization").unwrap(), "Bearer tok");
        assert_eq!(config.display_name(), "https://mcp.example.com/sse");
    }

    #[test]
    fn test_config_display_name_no_command_no_url() {
        let config = McpServerConfig {
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
        };
        assert_eq!(config.display_name(), "unknown");
    }

    #[test]
    fn test_config_defaults() {
        let json = r#"{}"#;
        let config: McpServerConfig = serde_json::from_str(json).unwrap();
        assert!(config.command.is_none());
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
        assert!(config.url.is_none());
        assert!(config.headers.is_empty());
    }

    // --- Schema validation ---

    #[test]
    fn test_validate_schema_simple_object() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "count": {"type": "integer"}
            }
        });
        assert!(validate_schema(&schema));
    }

    #[test]
    fn test_validate_schema_empty_object() {
        let schema = serde_json::json!({"type": "object"});
        assert!(validate_schema(&schema));
    }

    #[test]
    fn test_validate_schema_at_max_depth() {
        let mut schema = serde_json::json!({"type": "string"});
        for _ in 0..9 {
            schema = serde_json::json!({"nested": schema});
        }
        assert!(validate_schema(&schema));
    }

    #[test]
    fn test_validate_schema_exceeds_max_depth() {
        let mut schema = serde_json::json!({"type": "string"});
        for _ in 0..11 {
            schema = serde_json::json!({"nested": schema});
        }
        assert!(!validate_schema(&schema));
    }

    #[test]
    fn test_validate_schema_array_depth() {
        let mut schema = serde_json::json!({"type": "string"});
        for _ in 0..11 {
            schema = serde_json::json!([schema]);
        }
        assert!(!validate_schema(&schema));
    }

    #[test]
    fn test_validate_schema_exceeds_max_size() {
        let mut props = serde_json::Map::new();
        for i in 0..2000 {
            props.insert(
                format!("field_{i}_with_a_long_name_padding"),
                serde_json::json!({"type": "string", "description": "x".repeat(30)}),
            );
        }
        let schema = serde_json::Value::Object(props);
        assert!(!validate_schema(&schema));
    }

    #[test]
    fn test_validate_schema_scalar_values() {
        assert!(validate_schema(&serde_json::json!(null)));
        assert!(validate_schema(&serde_json::json!(42)));
        assert!(validate_schema(&serde_json::json!("hello")));
        assert!(validate_schema(&serde_json::json!(true)));
    }

    // --- JSON-RPC serialization/deserialization ---

    #[test]
    fn test_jsonrpc_request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: 42,
            method: "tools/list".into(),
            params: serde_json::json!({}),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 42);
        assert_eq!(json["method"], "tools/list");
        assert_eq!(json["params"], serde_json::json!({}));
    }

    #[test]
    fn test_jsonrpc_response_with_result() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, 1);
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_jsonrpc_response_with_error() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, 1);
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }

    #[test]
    fn test_jsonrpc_response_null_result() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.result.is_none());
    }

    // --- MCP tool definition deserialization ---

    #[test]
    fn test_mcp_tool_def_full() {
        let json = r#"{"name":"read","description":"Read a file","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}}}"#;
        let def: McpToolDef = serde_json::from_str(json).unwrap();
        assert_eq!(def.name, "read");
        assert_eq!(def.description.as_deref(), Some("Read a file"));
        assert!(def.input_schema.is_some());
    }

    #[test]
    fn test_mcp_tool_def_minimal() {
        let json = r#"{"name":"ping"}"#;
        let def: McpToolDef = serde_json::from_str(json).unwrap();
        assert_eq!(def.name, "ping");
        assert!(def.description.is_none());
        assert!(def.input_schema.is_none());
    }

    #[test]
    fn test_mcp_tool_list_response() {
        let json = r#"{"tools":[{"name":"a"},{"name":"b","description":"tool b"}]}"#;
        let resp: McpToolListResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.tools.len(), 2);
        assert_eq!(resp.tools[0].name, "a");
        assert_eq!(resp.tools[1].name, "b");
    }

    // --- BLOCKED_ENV_VARS filtering ---

    #[test]
    fn test_blocked_env_vars_contains_known_dangerous_vars() {
        let expected = [
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "NODE_OPTIONS",
            "PYTHONSTARTUP",
            "BASH_ENV",
        ];
        for var in &expected {
            assert!(
                BLOCKED_ENV_VARS.iter().any(|b| b == var),
                "{var} should be in BLOCKED_ENV_VARS"
            );
        }
    }

    #[test]
    fn test_blocked_env_vars_filtering_logic() {
        let env: HashMap<String, String> = [
            ("SAFE_VAR".into(), "ok".into()),
            ("LD_PRELOAD".into(), "evil.so".into()),
            ("NODE_OPTIONS".into(), "--require=bad".into()),
            ("MY_TOKEN".into(), "secret".into()),
        ]
        .into_iter()
        .collect();

        let allowed: Vec<&String> = env
            .keys()
            .filter(|k| {
                !BLOCKED_ENV_VARS
                    .iter()
                    .any(|blocked| k.eq_ignore_ascii_case(blocked))
            })
            .collect();

        assert!(allowed.contains(&&"SAFE_VAR".to_string()));
        assert!(allowed.contains(&&"MY_TOKEN".to_string()));
        assert!(!allowed.contains(&&"LD_PRELOAD".to_string()));
        assert!(!allowed.contains(&&"NODE_OPTIONS".to_string()));
    }

    #[test]
    fn test_blocked_env_vars_case_insensitive() {
        let key = "ld_preload";
        assert!(
            BLOCKED_ENV_VARS
                .iter()
                .any(|blocked| key.eq_ignore_ascii_case(blocked)),
            "filtering should be case-insensitive"
        );
    }

    // --- Protected names ---

    #[test]
    fn test_protected_names_coverage() {
        let names = McpClient::PROTECTED_NAMES;
        assert!(!names.is_empty());
        let mut seen = std::collections::HashSet::new();
        for name in names {
            assert!(!name.is_empty());
            assert!(seen.insert(name), "duplicate protected name: {name}");
        }
    }
}
