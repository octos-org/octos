//! CLI commands for octos.

mod account;
pub mod acp;
mod admin;
mod auth;
mod cache;
mod channels;
pub mod chat;
mod clean;
mod completions;
mod config;
mod cron;
mod docs;
mod doctor;
pub mod gateway;
mod goal;
mod host_patch;

mod inbox;

mod init;
mod ledger;
pub mod mcp;
pub mod mcp_serve;
mod memory;
pub(crate) mod obs_resolve;
mod office;
#[cfg(feature = "api")]
pub(crate) mod oup_client;
#[cfg(feature = "api")]
pub(crate) mod oup_peers;
#[cfg(feature = "api")]
pub(crate) mod oup_session;
#[cfg(feature = "api")]
mod oup_text;
#[cfg_attr(test, allow(unused_imports))]
mod peer;
mod profile;
#[cfg(feature = "api")]
mod serve;
pub mod serve_console;
pub mod skills;
pub(crate) mod skills_dev;
mod status;
mod steer;
mod update;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::Result;

pub use account::AccountCommand;
pub use acp::{AcpCommand, NotifyIfBusy, NotifyRequest, NotifyResponse};
// Test-support seam for the `octos acp` bridge: the end-to-end integration test
// in `crates/octos-cli/tests/acp_integration.rs` drives the real ACP handler
// wiring with a `MockLlm`-backed agent over an in-process transport. Hidden
// from docs; not part of the stable surface.
#[doc(hidden)]
#[cfg(feature = "api")]
pub use acp::{OctosAcpAgentTransport, TestAgentFactory};
pub use admin::AdminCommand;
pub use auth::AuthCommand;
pub use cache::CacheCommand;
pub use channels::ChannelsCommand;
pub use chat::ChatCommand;
pub use clean::CleanCommand;
pub use completions::CompletionsCommand;
pub use config::ConfigCommand;
pub use cron::CronCommand;
pub use docs::DocsCommand;
pub use doctor::DoctorCommand;
pub use gateway::GatewayCommand;
pub use goal::GoalCommand;
pub use host_patch::HostPatchCommand;

pub use inbox::InboxCommand;

pub use init::InitCommand;
pub use ledger::LedgerCommand;
pub use mcp::McpCommand;
pub use mcp_serve::McpServeCommand;
pub use memory::MemoryCommand;
pub(crate) use obs_resolve as obs;
pub use office::OfficeCommand;
#[cfg_attr(not(test), allow(unused_imports))]
pub use peer::PeerCommand;
pub use profile::ProfileCommand;
#[cfg(feature = "api")]
pub use serve::ServeCommand;
pub use skills::SkillsCommand;
pub use status::StatusCommand;
pub use steer::SteerCommand;
pub use update::UpdateCommand;

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
    /// Internal patch helper for a host-provided workspace sandbox.
    #[command(hide = true, name = "__host-patch")]
    HostPatch(HostPatchCommand),
    /// Manage sub-accounts under profiles.
    Account(AccountCommand),
    /// Run as an Agent Client Protocol (ACP) agent over stdio (Zed, etc.).
    Acp(AcpCommand),
    /// Admin commands for tenant and tunnel management.
    Admin(AdminCommand),
    /// Manage authentication for LLM providers.
    Auth(AuthCommand),
    /// Manage messaging channels.
    Channels(ChannelsCommand),
    /// Interactive multi-turn chat with an agent.
    Chat(ChatCommand),
    /// Inspect and reclaim the build-cache pool (`status` / `gc` / `gate`).
    Cache(CacheCommand),
    /// Inspect the saved startup config (`show` / `path`); read-only.
    Config(ConfigCommand),
    /// Manage scheduled cron jobs.
    Cron(CronCommand),
    /// Run local environment diagnostics (flutter-doctor style).
    Doctor(DoctorCommand),
    /// Generate documentation for tools and providers.
    Docs(DocsCommand),
    /// Initialize a new .octos configuration.
    Init(InitCommand),
    /// Query inbox notes file paths (read-only; OLP observability).
    Inbox(InboxCommand),
    /// Manage OAuth-authenticated MCP servers (`login`/`logout`).
    Mcp(McpCommand),
    /// Inspect and drive the memory-refresh pipeline.
    Memory(MemoryCommand),
    /// Portable profile export (QR) and payload inspection.
    Profile(ProfileCommand),
    /// Run as an MCP server so outer orchestrators can invoke octos as a sub-agent.
    McpServe(McpServeCommand),
    /// Start the REST API server (requires --features api).
    #[cfg(feature = "api")]
    Serve(ServeCommand),
    /// Manage agent skills (list, install, remove, develop from source).
    Skills(SkillsCommand),
    /// Show system status.
    Status(StatusCommand),
    /// Queue an external-reviewer steer into a session (OLP control).
    Steer(SteerCommand),
    /// Check for a newer octos release (`--check`); self-update is Stage 3.
    Update(UpdateCommand),
    /// Run as a persistent messaging gateway.
    Gateway(GatewayCommand),

    /// Operator goal transitions (reopen a blocked/paused goal, archive a goal terminally).
    Goal(GoalCommand),
    /// Read-only goal-ledger tail (findings/escalations/decisions; OLP).
    Ledger(LedgerCommand),

    /// Clean up stale state and cache files.
    Clean(CleanCommand),
    /// Generate shell completions.
    Completions(CompletionsCommand),
    /// Office file manipulation (extract, unpack, pack, clean, add-slide, validate).
    Office(OfficeCommand),
    /// Read-only peer listing (direct peers/ dir read; OLP observability).
    Peer(PeerCommand),
}

/// Whether stdout is reserved for protocol or assistant output, so tracing
/// must use stderr instead of interleaving log lines with that output.
///
/// * `acp` speaks ACP JSON-RPC on stdout (one stray log line → a `-32700`
///   parse error at strict clients like Zed);
/// * `mcp-serve --transport stdio` speaks MCP JSON-RPC on stdout;
/// * `profile` emits payloads meant for `$(...)` capture / piping;
/// * `chat` streams assistant text (or one `--json` result) on stdout;
/// * `doctor --json` emits the diagnostics support bundle on stdout — the
///   config-parse check loads the real config, whose tracing INFO lines
///   ("no config.json found, using defaults") would otherwise corrupt the
///   JSON.
///
/// Every other command keeps its historical stdout console routing untouched.
pub fn reserve_stdout(command: &Command) -> bool {
    match command {
        Command::Acp(_) | Command::Profile(_) | Command::McpServe(_) | Command::Chat(_) => true,
        // `inbox path` is a machine-readable single path.
        Command::Inbox(_) => true,
        Command::Doctor(cmd) => cmd.json,
        // `octos cache <sub> --json` emits a machine-readable object meant
        // for scripting / the outer loop's gate check — same rule as
        // `doctor --json`. Without `--json` the human table stays on the
        // historical stdout routing.
        Command::Cache(cmd) => cmd.emits_json(),
        _ => false,
    }
}

/// Trait for executable commands (following dora-rs pattern).
pub trait Executable {
    fn execute(self) -> Result<()>;
}

/// Resolve the data directory for episodes, memory, sessions, etc.
///
/// Priority: `--data-dir` CLI flag > `OCTOS_HOME` env var > `~/.octos` default.
/// Delegates to the canonical [`resolve_config_context`] so the `data_dir`
/// computation (including empty-string env handling) never diverges from
/// config/auth resolution.
pub fn resolve_data_dir(cli_override: Option<PathBuf>) -> eyre::Result<PathBuf> {
    let ctx = crate::config_context::resolve_config_context(cli_override.as_deref());
    std::fs::create_dir_all(&ctx.data_dir).ok();
    Ok(ctx.data_dir)
}

/// Build the `profile '<id>' not found` message, listing the ids that do
/// exist so a typo doesn't dead-end the user (#2414).
pub fn profile_not_found_message(profiles: &[crate::profiles::UserProfile], id: &str) -> String {
    if profiles.is_empty() {
        return format!("profile '{id}' not found. No profiles are configured.");
    }
    let ids: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
    format!(
        "profile '{id}' not found. Existing profiles: {}",
        ids.join(", ")
    )
}

/// Resolve the canonical [`ConfigContext`](crate::config_context::ConfigContext)
/// for a command, create the data dir, and run the (idempotent, best-effort)
/// config + auth migrations exactly once.
///
/// Every command that loads config should go through this so the resolver runs
/// at a single shared entrypoint per invocation.
pub fn resolve_command_context(
    cli_override: Option<PathBuf>,
) -> eyre::Result<crate::config_context::ConfigContext> {
    let ctx = crate::config_context::resolve_config_context(cli_override.as_deref());
    std::fs::create_dir_all(&ctx.data_dir).ok();
    crate::config_context::run_migrations(&ctx);
    Ok(ctx)
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

/// M8.3: load a profile's `system_prompt_template` hint.
///
/// The path is treated as relative to `~/.octos/profiles/<profile_name>/`.
/// Missing files are not an error — we log and return `None` so the agent
/// keeps its default prompt. Empty files are also treated as missing.
pub(crate) fn load_profile_prompt_template(
    profile_name: &str,
    template_rel: &std::path::Path,
) -> Option<String> {
    let home = dirs::home_dir()?;
    let base = home.join(".octos/profiles").join(profile_name);
    let path = base.join(template_rel);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                tracing::warn!(
                    path = %path.display(),
                    "profile system_prompt_template exists but is empty; using default prompt"
                );
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "profile system_prompt_template not found; using default prompt"
            );
            None
        }
    }
}

impl Executable for Command {
    fn execute(self) -> Result<()> {
        match self {
            Self::HostPatch(cmd) => cmd.execute(),
            Self::Account(cmd) => cmd.execute(),
            Self::Acp(cmd) => cmd.execute(),
            Self::Admin(cmd) => cmd.execute(),
            Self::Auth(cmd) => cmd.execute(),
            Self::Channels(cmd) => cmd.execute(),
            Self::Chat(cmd) => cmd.execute(),
            Self::Cache(cmd) => cmd.execute(),
            Self::Config(cmd) => cmd.execute(),
            Self::Cron(cmd) => cmd.execute(),
            Self::Doctor(cmd) => cmd.execute(),
            Self::Docs(cmd) => cmd.execute(),
            Self::Init(cmd) => cmd.execute(),
            Self::Inbox(cmd) => cmd.execute(),
            Self::Mcp(cmd) => cmd.execute(),
            Self::Profile(cmd) => cmd.execute(),
            Self::McpServe(cmd) => cmd.execute(),
            #[cfg(feature = "api")]
            Self::Serve(cmd) => cmd.execute(),
            Self::Skills(cmd) => cmd.execute(),
            Self::Status(cmd) => cmd.execute(),
            Self::Steer(cmd) => cmd.execute(),
            Self::Update(cmd) => cmd.execute(),
            Self::Gateway(cmd) => cmd.execute(),
            Self::Goal(cmd) => cmd.execute(),
            Self::Ledger(cmd) => cmd.execute(),
            Self::Clean(cmd) => cmd.execute(),
            Self::Memory(cmd) => cmd.execute(),
            Self::Completions(cmd) => cmd.execute(),
            Self::Office(cmd) => cmd.execute(),

            Self::Peer(cmd) => cmd.execute(),
        }
    }
}
