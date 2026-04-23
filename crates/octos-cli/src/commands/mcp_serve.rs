//! M7.2 — `octos mcp-serve` subcommand.
//!
//! Exposes octos as an MCP server so outer orchestrators can invoke it
//! as a sub-agent. See [`octos_agent::mcp_server`] for the transport and
//! session-level tool semantics.
//!
//! # Transports
//!
//! - `stdio` (default): JSON-RPC over stdin/stdout. Parent-trust auth.
//! - `http`: minimal HTTP/1.1 JSON-RPC endpoint. Requires a bearer token
//!   supplied via the `OCTOS_MCP_SERVER_TOKEN` environment variable.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use clap::{Args, ValueEnum};
use eyre::{Result, WrapErr};
use octos_agent::HarnessEvent;
use octos_agent::mcp_server::{
    McpServer, McpServerError, McpSessionDispatch, McpSessionOutcome, OCTOS_MCP_SERVER_TOKEN_ENV,
    SessionLifecycleObserver,
};
use octos_agent::task_supervisor::{TaskLifecycleState, TaskSupervisor};
use serde_json::Value;

use super::Executable;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpTransport {
    Stdio,
    Http,
}

/// Run octos as an MCP server for outer orchestrators.
#[derive(Debug, Args)]
pub struct McpServeCommand {
    /// Transport to bind. `stdio` (default) uses parent-trust auth; `http`
    /// requires a bearer token via `OCTOS_MCP_SERVER_TOKEN`.
    #[arg(long, value_enum, default_value_t = McpTransport::Stdio)]
    pub transport: McpTransport,

    /// Bind address for the HTTP transport. Defaults to 127.0.0.1:0 (ephemeral).
    #[arg(long, default_value = "127.0.0.1:4033")]
    pub bind: SocketAddr,

    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

impl Executable for McpServeCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024)
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

impl McpServeCommand {
    async fn run_async(self) -> Result<()> {
        // Session dispatch: hand work to the existing agent loop. The current
        // implementation is a thin adapter that records `contract` + `input`
        // and surfaces an error instructing the operator to wire in the
        // gateway-backed runner. Production integration is tracked in M7.3.
        let dispatch: Arc<dyn McpSessionDispatch> = Arc::new(DefaultSessionDispatch::new());
        let supervisor = Arc::new(TaskSupervisor::new());
        let server = McpServer::new(dispatch, supervisor);

        // Install a lightweight event sink that forwards typed harness events
        // to the tracing subsystem. Operators can pipe logs into the same
        // harness audit tooling the rest of the runtime uses.
        server
            .set_event_sink(|event: HarnessEvent| {
                tracing::info!(target: "mcp_serve_audit", ?event, "mcp-serve audit");
            })
            .await;

        match self.transport {
            McpTransport::Stdio => {
                tracing::info!("octos mcp-serve: stdio transport");
                server.serve_stdio().await
            }
            McpTransport::Http => {
                let token = std::env::var(OCTOS_MCP_SERVER_TOKEN_ENV).map_err(|_| {
                    eyre::eyre!("{OCTOS_MCP_SERVER_TOKEN_ENV} must be set for the http transport")
                })?;
                if token.trim().is_empty() {
                    eyre::bail!("{OCTOS_MCP_SERVER_TOKEN_ENV} must not be empty");
                }
                tracing::info!(
                    bind = %self.bind,
                    "octos mcp-serve: http transport (bearer token required)",
                );
                serve_http(server, self.bind, token).await
            }
        }
    }
}

/// Spawn the HTTP listener on `bind`, using the bearer token for authentication.
async fn serve_http(server: McpServer, bind: SocketAddr, token: String) -> Result<()> {
    let handle = server
        .serve_http(bind, token)
        .await
        .wrap_err_with(|| format!("failed to bind MCP server on {bind}"))?;
    tracing::info!(addr = %handle.addr(), "octos mcp-serve http bound");
    // Block on Ctrl+C or SIGTERM so the listener stays up for the process lifetime.
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    ctrl_c.await;
    handle.shutdown().await;
    Ok(())
}

/// Placeholder dispatch. Until the gateway/session runner is wired in
/// ([M7.3](https://github.com/octos-org/octos/issues/510)), any call returns a
/// structured failure so outer orchestrators see a deterministic error rather
/// than a partial session.
struct DefaultSessionDispatch;

impl DefaultSessionDispatch {
    fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpSessionDispatch for DefaultSessionDispatch {
    async fn run_session(
        &self,
        contract: &str,
        _input: &Value,
        _observer: &dyn SessionLifecycleObserver,
    ) -> Result<McpSessionOutcome, McpServerError> {
        Ok(McpSessionOutcome {
            final_state: TaskLifecycleState::Failed,
            artifact_path: None,
            artifact_content: None,
            validator_results: Vec::new(),
            cost: serde_json::json!({"input_tokens": 0, "output_tokens": 0}),
            error: Some(format!(
                "octos mcp-serve: session runner for contract '{contract}' is not wired in this build"
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_transport_parses() {
        let cmd: McpServeCommand = McpServeCommand {
            transport: McpTransport::Http,
            bind: "127.0.0.1:4033".parse().unwrap(),
            cwd: None,
            data_dir: None,
            config: None,
        };
        assert!(matches!(cmd.transport, McpTransport::Http));
    }

    #[test]
    fn default_dispatch_always_fails_until_wired() {
        // Sanity: the placeholder should never silently succeed.
        let dispatch = DefaultSessionDispatch::new();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let outcome = dispatch
                .run_session(
                    "slides_delivery",
                    &serde_json::json!({}),
                    &octos_agent::mcp_server::RecordingObserver::new(),
                )
                .await
                .expect("dispatch returns Ok with Failed outcome");
            assert_eq!(outcome.final_state, TaskLifecycleState::Failed);
            assert!(outcome.error.is_some());
        });
    }
}
