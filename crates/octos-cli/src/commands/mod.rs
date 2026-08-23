//! CLI commands for octos.

mod account;
pub mod acp;
mod admin;
mod auth;
mod channels;
pub mod chat;
mod clean;
mod completions;
mod config;
mod cron;
mod docs;
mod doctor;
pub mod gateway;
mod inbox;
mod init;
pub mod mcp;
pub mod mcp_serve;
mod memory;
mod office;
mod profile;
#[cfg(feature = "api")]
mod serve;
pub mod skills;
mod status;
mod update;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::Result;

pub use account::AccountCommand;
pub use acp::AcpCommand;
// Test-support seam for the `octos acp` bridge: the end-to-end integration test
// in `crates/octos-cli/tests/acp_integration.rs` drives the real ACP handler
// wiring with a `MockLlm`-backed agent over an in-process transport. Hidden
// from docs; not part of the stable surface.
#[doc(hidden)]
pub use acp::{OctosAcpAgentTransport, TestAgentFactory};
pub use admin::AdminCommand;
pub use auth::AuthCommand;
pub use channels::ChannelsCommand;
pub use chat::ChatCommand;
pub use clean::CleanCommand;
pub use completions::CompletionsCommand;
pub use config::ConfigCommand;
pub use cron::CronCommand;
pub use docs::DocsCommand;
pub use doctor::DoctorCommand;
pub use gateway::GatewayCommand;
pub use inbox::InboxCommand;
pub use init::InitCommand;
pub use mcp::McpCommand;
pub use mcp_serve::McpServeCommand;
pub use memory::MemoryCommand;
pub use office::OfficeCommand;
pub use profile::ProfileCommand;
#[cfg(feature = "api")]
pub use serve::ServeCommand;
pub use skills::SkillsCommand;
pub use status::StatusCommand;
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
    /// Manage agent skills (list, install, remove).
    Skills(SkillsCommand),
    /// Show system status.
    Status(StatusCommand),
    /// Check for a newer octos release (`--check`); self-update is Stage 3.
    Update(UpdateCommand),
    /// Run as a persistent messaging gateway.
    Gateway(GatewayCommand),
    /// Clean up stale state and cache files.
    Clean(CleanCommand),
    /// Generate shell completions.
    Completions(CompletionsCommand),
    /// Office file manipulation (extract, unpack, pack, clean, add-slide, validate).
    Office(OfficeCommand),
}

/// Whether `command` emits machine-readable output on stdout and therefore
/// needs the tracing console layer routed to stderr so logs never corrupt that
/// stream.
///
/// * `acp` speaks ACP JSON-RPC on stdout (one stray log line → a `-32700`
///   parse error at strict clients like Zed);
/// * `mcp-serve --transport stdio` speaks MCP JSON-RPC on stdout;
/// * `profile` emits payloads meant for `$(...)` capture / piping;
/// * `chat --json` emits a single JSON result object on stdout (scripting /
///   agent-to-agent);
/// * `doctor --json` emits the diagnostics support bundle on stdout — the
///   config-parse check loads the real config, whose tracing INFO lines
///   ("no config.json found, using defaults") would otherwise corrupt the
///   JSON.
///
/// Every other command keeps its historical stdout console routing untouched.
pub fn reserve_stdout(command: &Command) -> bool {
    match command {
        Command::Acp(_) | Command::Profile(_) | Command::McpServe(_) => true,
        // `inbox path` prints a single path meant for $(...) capture.
        Command::Inbox(_) => true,
        Command::Chat(cmd) => cmd.json,
        Command::Doctor(cmd) => cmd.json,
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

/// Build a [`PersistentCredentialPool`](octos_llm::PersistentCredentialPool)
/// from top-level `CredentialPoolConfig` (F-005). Returns `None` when
/// the config is absent, has no declared `credential_ids`, or when
/// opening the redb file fails — the caller falls back to the legacy
/// single-credential flow in that case. Errors are logged but never
/// fatal so a broken pool configuration cannot brick `octos serve`.
pub(crate) fn build_credential_pool(
    config: Option<&crate::config::CredentialPoolConfig>,
    data_dir: &std::path::Path,
) -> Option<std::sync::Arc<octos_llm::PersistentCredentialPool>> {
    let cfg = config?;
    if cfg.credential_ids.is_empty() {
        tracing::debug!("credential pool config present but `credential_ids` empty; skipping");
        return None;
    }

    let strategy = match cfg.strategy.as_str() {
        "fill_first" => octos_llm::RotationStrategy::FillFirst,
        "round_robin" => octos_llm::RotationStrategy::RoundRobin,
        "random" => octos_llm::RotationStrategy::Random,
        "least_used" => octos_llm::RotationStrategy::LeastUsed,
        other => {
            tracing::warn!(
                strategy = other,
                "unknown credential pool strategy; defaulting to round_robin"
            );
            octos_llm::RotationStrategy::RoundRobin
        }
    };

    let credentials: Vec<octos_llm::Credential> = cfg
        .credential_ids
        .iter()
        .map(|id| {
            // Env var lookup: <ID>_API_KEY uppercased. Missing env →
            // empty secret. The provider adapter will surface the
            // explicit auth error on first use rather than at startup.
            let env_var = format!("{}_API_KEY", id.replace('-', "_").to_uppercase());
            let secret = std::env::var(&env_var).unwrap_or_default();
            octos_llm::Credential::new(id.clone(), secret)
        })
        .collect();

    let path = cfg
        .state_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| data_dir.join(octos_llm::DEFAULT_CREDENTIAL_POOL_DB_FILENAME));

    let mut options =
        octos_llm::PersistentCredentialPoolOptions::new(cfg.name.clone(), credentials)
            .with_strategy(strategy);
    if let Some(ms) = cfg.default_cooldown_ms {
        options = options.with_default_cooldown_us(ms.saturating_mul(1_000));
    }

    match octos_llm::PersistentCredentialPool::open(&path, options) {
        Ok(pool) => {
            tracing::info!(
                path = %path.display(),
                name = %cfg.name,
                strategy = %cfg.strategy,
                "credential pool opened"
            );
            Some(std::sync::Arc::new(pool))
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to open credential pool; falling back to single-credential flow"
            );
            None
        }
    }
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
            Self::Account(cmd) => cmd.execute(),
            Self::Acp(cmd) => cmd.execute(),
            Self::Admin(cmd) => cmd.execute(),
            Self::Auth(cmd) => cmd.execute(),
            Self::Channels(cmd) => cmd.execute(),
            Self::Chat(cmd) => cmd.execute(),
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
            Self::Update(cmd) => cmd.execute(),
            Self::Gateway(cmd) => cmd.execute(),
            Self::Clean(cmd) => cmd.execute(),
            Self::Memory(cmd) => cmd.execute(),
            Self::Completions(cmd) => cmd.execute(),
            Self::Office(cmd) => cmd.execute(),
        }
    }
}

#[cfg(test)]
mod reserve_stdout_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn should_reserve_stdout_when_chat_json_set() {
        // `octos chat --json` opts into a pure-stdout JSON result stream, so
        // its tracing logs must route to stderr.
        let args = Args::try_parse_from(["octos", "chat", "--json", "--message", "hi"])
            .expect("`chat --json` must parse");
        assert!(reserve_stdout(&args.command));
    }

    #[test]
    fn should_not_reserve_stdout_when_chat_lacks_json() {
        // Plain `octos chat` keeps its historical stdout console routing.
        let args =
            Args::try_parse_from(["octos", "chat", "--message", "hi"]).expect("`chat` must parse");
        assert!(!reserve_stdout(&args.command));
    }

    #[test]
    fn should_reserve_stdout_only_for_doctor_json() {
        // `octos doctor --json` emits the support bundle on stdout; the
        // config-parse check's tracing INFO lines must route to stderr so the
        // JSON stays parseable. Plain `octos doctor` keeps stdout logging.
        let json = Args::try_parse_from(["octos", "doctor", "--json"])
            .expect("`doctor --json` must parse");
        assert!(reserve_stdout(&json.command));
        let human = Args::try_parse_from(["octos", "doctor"]).expect("`doctor` must parse");
        assert!(!reserve_stdout(&human.command));
    }

    #[test]
    fn should_not_reserve_stdout_for_ordinary_command() {
        // A non-protocol command (e.g. `status`) is unchanged by the chat-json
        // extension — Serve, Status, etc. never reserve stdout.
        let args = Args::try_parse_from(["octos", "status"]).expect("`status` must parse");
        assert!(!reserve_stdout(&args.command));
    }

    #[test]
    fn should_reserve_stdout_for_stdio_protocol_commands() {
        // Pre-existing reservations must remain: acp / mcp-serve speak a
        // machine protocol on stdout.
        let acp = Args::try_parse_from(["octos", "acp"]).expect("`acp` must parse");
        assert!(reserve_stdout(&acp.command));
        let mcp = Args::try_parse_from(["octos", "mcp-serve"]).expect("`mcp-serve` must parse");
        assert!(reserve_stdout(&mcp.command));
    }
}
