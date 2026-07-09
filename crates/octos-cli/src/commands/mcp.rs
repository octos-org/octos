//! `octos mcp` — manage OAuth-authenticated MCP servers.
//!
//! `octos mcp login <url>` runs the OAuth 2.1 authorization-code flow against a
//! streamable-HTTP MCP server (browser consent + loopback redirect) and stores
//! the resulting tokens in the OS keyring. The agent runtime then loads/refreshes
//! them automatically for any `mcp_servers` entry with `oauth = true`.

use clap::{Args, Subcommand};
use eyre::{Result, WrapErr};

use super::Executable;

#[derive(Debug, Args)]
pub struct McpCommand {
    #[command(subcommand)]
    pub action: McpAction,
}

#[derive(Debug, Subcommand)]
pub enum McpAction {
    /// Authorize octos against an OAuth-gated MCP server (browser consent).
    Login {
        /// The MCP server URL (e.g. https://host/mcp).
        url: String,
        /// OAuth scope to request (repeatable). Server-specific.
        #[arg(long = "scope")]
        scopes: Vec<String>,
    },
    /// Remove stored OAuth tokens for an MCP server.
    Logout {
        /// The MCP server URL previously logged in.
        url: String,
    },
}

impl Executable for McpCommand {
    fn execute(self) -> Result<()> {
        match self.action {
            McpAction::Login { url, scopes } => tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .wrap_err("failed to create tokio runtime")?
                .block_on(octos_agent::mcp_auth::login(&url, &scopes))
                .wrap_err_with(|| format!("MCP login failed for {url}")),
            McpAction::Logout { url } => {
                let removed = octos_agent::mcp_auth::delete_tokens(&url)
                    .wrap_err_with(|| format!("MCP logout failed for {url}"))?;
                if removed {
                    println!("Removed stored OAuth tokens for {url}.");
                } else {
                    println!("No stored OAuth tokens for {url}.");
                }
                Ok(())
            }
        }
    }
}
