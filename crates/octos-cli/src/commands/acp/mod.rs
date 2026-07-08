//! ACP (Agent Communication Protocol) bridge command.
//!
//! Provides a gateway-backed ACP bridge for agent communication.

mod bridge;
mod acp;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use eyre::Result;
use tracing::{error, info};
use url::Url;

use bridge::{AcpBridge, BridgeConfig};

use super::Executable;

/// ACP bridge command - run an ACP bridge backed by the Gateway.
#[derive(Debug, Args)]
pub struct AcpCommand {
    /// Do not prefix prompts with the working directory
    #[arg(long)]
    pub no_prefix_cwd: bool,

    /// Gateway password (if required)
    #[arg(long)]
    pub password: Option<String>,

    /// Read gateway password from file
    #[arg(long)]
    pub password_file: Option<PathBuf>,

    /// Fail if the session key/label does not exist
    #[arg(long, default_value = "false")]
    pub require_existing: bool,

    /// Reset the session key before first use
    #[arg(long, default_value = "false")]
    pub reset_session: bool,

    /// Default session key (e.g. agent:main:main)
    #[arg(long)]
    pub session: Option<String>,

    /// Default session label to resolve
    #[arg(long)]
    pub session_label: Option<String>,

    /// Gateway token (if required)
    #[arg(long)]
    pub token: Option<String>,

    /// Read gateway token from file
    #[arg(long)]
    pub token_file: Option<PathBuf>,

    /// Gateway WebSocket URL (defaults to gateway.remote.url when configured)
    #[arg(long)]
    pub url: Option<Url>,

    /// Verbose logging
    #[arg(short, long)]
    pub verbose: bool,

    #[command(subcommand)]
    pub subcommand: AcpSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum AcpSubcommand {
    /// Run an interactive ACP client against the local ACP bridge
    Client(ClientArgs),
}

#[derive(Debug, Args)]
pub struct ClientArgs {
    /// Run in interactive mode
    #[arg(short, long, default_value = "true")]
    pub interactive: bool,
}

impl Executable for AcpCommand {
    fn execute(self) -> Result<()> {
        // Build tokio runtime for async execution
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async { run_acp_command(self).await })
    }
}

async fn run_acp_command(args: AcpCommand) -> Result<()> {
    info!("Starting Octos ACP Bridge");

    // Load base configuration
    let mut config = BridgeConfig::from_env();

    // Apply command line arguments to config
    apply_acp_args_to_config(&mut config, &args)?;

    // Create and initialize bridge
    let mut bridge = AcpBridge::new(config);
    bridge.initialize().await?;

    // Execute the ACP subcommand
    match args.subcommand {
        AcpSubcommand::Client(client_args) => {
            info!(
                "Starting ACP client (interactive: {})",
                client_args.interactive
            );
            bridge.run_client().await?;
        }
    }

    info!("Octos ACP Bridge shutdown complete");
    Ok(())
}

fn apply_acp_args_to_config(config: &mut BridgeConfig, args: &AcpCommand) -> Result<()> {
    // Apply URL if provided
    if let Some(url) = &args.url {
        config.gateway_url = url.clone();
    }

    // Apply authentication options
    if let Some(token) = &args.token {
        config.token = Some(token.clone());
    }

    if let Some(password) = &args.password {
        config.password = Some(password.clone());
    }

    // Load token from file if specified
    if let Some(token_file) = &args.token_file {
        match std::fs::read_to_string(token_file) {
            Ok(token) => {
                config.token = Some(token.trim().to_string());
                info!("Loaded token from file: {:?}", token_file);
            }
            Err(e) => {
                error!("Failed to read token file {:?}: {}", token_file, e);
                return Err(e.into());
            }
        }
    }

    // Load password from file if specified
    if let Some(password_file) = &args.password_file {
        match std::fs::read_to_string(password_file) {
            Ok(password) => {
                config.password = Some(password.trim().to_string());
                info!("Loaded password from file: {:?}", password_file);
            }
            Err(e) => {
                error!("Failed to read password file {:?}: {}", password_file, e);
                return Err(e.into());
            }
        }
    }

    // Apply session options
    if let Some(session) = &args.session {
        config.session_key = Some(session.clone());
    }

    if let Some(session_label) = &args.session_label {
        config.session_label = Some(session_label.clone());
    }

    // Apply behavior flags
    config.verbose = args.verbose;
    config.require_existing = args.require_existing;
    config.reset_session = args.reset_session;
    config.no_prefix_cwd = args.no_prefix_cwd;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestArgs {
        #[command(subcommand)]
        command: TestCommand,
    }

    #[derive(Subcommand)]
    enum TestCommand {
        Acp(AcpCommand),
    }

    #[test]
    fn test_basic_acp_client_command() {
        let args = TestArgs::parse_from(["test", "acp", "client"]);
        if let TestCommand::Acp(acp) = args.command {
            assert!(matches!(acp.subcommand, AcpSubcommand::Client(_)));
        } else {
            panic!("Expected Acp command");
        }
    }

    #[test]
    fn test_custom_session() {
        let args = TestArgs::parse_from(["test", "acp", "--session", "agent:dev:test", "client"]);
        if let TestCommand::Acp(acp) = args.command {
            assert_eq!(acp.session, Some("agent:dev:test".to_string()));
        } else {
            panic!("Expected Acp command");
        }
    }

    #[test]
    fn test_gateway_url() {
        let args = TestArgs::parse_from([
            "test",
            "acp",
            "--url",
            "wss://gateway.example.com/acp",
            "client",
        ]);
        if let TestCommand::Acp(acp) = args.command {
            assert!(acp.url.is_some());
            assert_eq!(
                acp.url.unwrap().as_str(),
                "wss://gateway.example.com/acp"
            );
        } else {
            panic!("Expected Acp command");
        }
    }
}