//! CLI commands for octos.

mod account;
mod admin;
mod auth;
mod channels;
pub(crate) mod chat;
mod clean;
mod completions;
mod cron;
mod docs;
pub(crate) mod gateway;
mod init;
mod mcp_serve;
mod office;
#[cfg(feature = "api")]
mod serve;
pub(crate) mod skills;
mod status;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::Result;

pub use account::AccountCommand;
pub use admin::AdminCommand;
pub use auth::AuthCommand;
pub use channels::ChannelsCommand;
pub use chat::ChatCommand;
pub use clean::CleanCommand;
pub use completions::CompletionsCommand;
pub use cron::CronCommand;
pub use docs::DocsCommand;
pub use gateway::GatewayCommand;
pub use init::InitCommand;
pub use mcp_serve::McpServeCommand;
pub use office::OfficeCommand;
#[cfg(feature = "api")]
pub use serve::ServeCommand;
pub use skills::SkillsCommand;
pub use status::StatusCommand;

/// octos: Rust-native coding agent orchestration.
#[derive(Debug, Parser)]
#[command(name = "octos")]
#[command(author, about, long_about = None)]
#[command(version = version_string())]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

/// Build a version string like "0.1.0 (abc1234 2026-03-02)".
fn version_string() -> &'static str {
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    const GIT_HASH: &str = match option_env!("OCTOS_GIT_HASH") {
        Some(v) => v,
        None => "",
    };
    const BUILD_DATE: &str = match option_env!("OCTOS_BUILD_DATE") {
        Some(v) => v,
        None => "",
    };

    // Leak a formatted string so we get a &'static str for clap
    #[allow(clippy::const_is_empty)]
    if GIT_HASH.is_empty() {
        VERSION
    } else {
        Box::leak(format!("{VERSION} ({GIT_HASH} {BUILD_DATE})").into_boxed_str())
    }
}

/// Available commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage sub-accounts under profiles.
    Account(AccountCommand),
    /// Admin commands for tenant and tunnel management.
    Admin(AdminCommand),
    /// Manage authentication for LLM providers.
    Auth(AuthCommand),
    /// Manage messaging channels.
    Channels(ChannelsCommand),
    /// Interactive multi-turn chat with an agent.
    Chat(ChatCommand),
    /// Manage scheduled cron jobs.
    Cron(CronCommand),
    /// Generate documentation for tools and providers.
    Docs(DocsCommand),
    /// Initialize a new .octos configuration.
    Init(InitCommand),
    /// Run as an MCP server so outer orchestrators can invoke octos as a sub-agent.
    McpServe(McpServeCommand),
    /// Start the REST API server (requires --features api).
    #[cfg(feature = "api")]
    Serve(ServeCommand),
    /// Manage agent skills (list, install, remove).
    Skills(SkillsCommand),
    /// Show system status.
    Status(StatusCommand),
    /// Run as a persistent messaging gateway.
    Gateway(GatewayCommand),
    /// Clean up stale state and cache files.
    Clean(CleanCommand),
    /// Generate shell completions.
    Completions(CompletionsCommand),
    /// Office file manipulation (extract, unpack, pack, clean, add-slide, validate).
    Office(OfficeCommand),
}

/// Trait for executable commands (following dora-rs pattern).
pub trait Executable {
    fn execute(self) -> Result<()>;
}

/// Resolve the data directory for episodes, memory, sessions, etc.
///
/// Priority: `--data-dir` CLI flag > `OCTOS_HOME` env var > `~/.octos` default.
pub(crate) fn resolve_data_dir(cli_override: Option<PathBuf>) -> eyre::Result<PathBuf> {
    let dir = if let Some(d) = cli_override {
        d
    } else if let Ok(env_dir) = std::env::var("OCTOS_HOME") {
        PathBuf::from(env_dir)
    } else {
        dirs::home_dir()
            .ok_or_else(|| eyre::eyre!("cannot determine home directory"))?
            .join(".octos")
    };
    std::fs::create_dir_all(&dir).ok();
    Ok(dir)
}

/// Load a prompt from `~/.octos/prompts/{name}.md` at runtime.
/// Falls back to `compiled_default` if the file doesn't exist or is empty.
pub(crate) fn load_prompt(name: &str, compiled_default: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".octos/prompts").join(format!("{name}.md"));
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    compiled_default.to_string()
}

/// Load optional bootstrap/personality files from the .octos/ directory.
/// Used by both chat and gateway to build the system prompt from AGENTS.md, SOUL.md, etc.
pub(crate) fn load_bootstrap_files(data_dir: &std::path::Path) -> String {
    const FILES: &[&str] = &["AGENTS.md", "SOUL.md", "USER.md", "TOOLS.md", "IDENTITY.md"];
    let mut parts = Vec::new();
    for filename in FILES {
        let path = data_dir.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                parts.push(format!("## {filename}\n\n{trimmed}"));
            }
        }
    }
    parts.join("\n\n")
}

impl Executable for Command {
    fn execute(self) -> Result<()> {
        match self {
            Self::Account(cmd) => cmd.execute(),
            Self::Admin(cmd) => cmd.execute(),
            Self::Auth(cmd) => cmd.execute(),
            Self::Channels(cmd) => cmd.execute(),
            Self::Chat(cmd) => cmd.execute(),
            Self::Cron(cmd) => cmd.execute(),
            Self::Docs(cmd) => cmd.execute(),
            Self::Init(cmd) => cmd.execute(),
            Self::McpServe(cmd) => cmd.execute(),
            #[cfg(feature = "api")]
            Self::Serve(cmd) => cmd.execute(),
            Self::Skills(cmd) => cmd.execute(),
            Self::Status(cmd) => cmd.execute(),
            Self::Gateway(cmd) => cmd.execute(),
            Self::Clean(cmd) => cmd.execute(),
            Self::Completions(cmd) => cmd.execute(),
            Self::Office(cmd) => cmd.execute(),
        }
    }
}
