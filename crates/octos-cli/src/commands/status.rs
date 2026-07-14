//! Status command: show system status.

use std::path::PathBuf;

use clap::Args;
use colored::Colorize;
use eyre::{Result, WrapErr};

use super::Executable;
use crate::config::Config;

/// Show system status.
#[derive(Debug, Args)]
pub struct StatusCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,
}

impl Executable for StatusCommand {
    fn execute(self) -> Result<()> {
        let cwd = match self.cwd {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };
        show_system_status(&cwd)
    }
}

/// Known provider environment variable names.
const PROVIDER_ENV_VARS: &[(&str, &str)] = &[
    ("Anthropic", "ANTHROPIC_API_KEY"),
    ("OpenAI", "OPENAI_API_KEY"),
    ("Gemini", "GEMINI_API_KEY"),
    ("OpenRouter", "OPENROUTER_API_KEY"),
    ("DeepSeek", "DEEPSEEK_API_KEY"),
    ("Groq", "GROQ_API_KEY"),
    ("Moonshot", "KIMI_API_KEY"),
    ("DashScope", "DASHSCOPE_API_KEY"),
    ("MiniMax", "MINIMAX_API_KEY"),
    ("Zhipu", "ZHIPU_API_KEY"),
];

fn show_system_status(cwd: &std::path::Path) -> Result<()> {
    println!("{}", "octos Status".cyan().bold());
    println!("{}", "═".repeat(50));
    println!();

    let config_path = cwd.join(".octos").join("config.json");
    let ctx = super::resolve_command_context(None)?;
    let data_dir = ctx.data_dir.clone();
    let config_home_config = ctx.config_home.join("config.json");
    // Legacy back-compat location (default installs only).
    let legacy_config = dirs::home_dir().map(|h| h.join(".octos").join("config.json"));

    // Config location — report the ACTUAL resolved config_home, not the data
    // dir, so the operator sees where config really lives (XDG by default).
    // Project-local `cwd/.octos/config.json` is only honored by the loader in a
    // DEFAULT context (`load_resolved` skips it for explicit/tenant), so only
    // surface it when `ctx.is_default` — else status would claim a project-local
    // config that the loader will not read.
    if ctx.is_default && config_path.exists() {
        println!(
            "{}: {} {}",
            "Config".green(),
            config_path.display(),
            "(found)".green()
        );
    } else if config_home_config.exists() {
        println!(
            "{}: {} {}",
            "Config".green(),
            config_home_config.display(),
            "(found)".green()
        );
    } else if ctx.is_default
        && legacy_config
            .as_deref()
            .map(|p| p != config_home_config && p.exists())
            .unwrap_or(false)
    {
        println!(
            "{}: {} {}",
            "Config".green(),
            legacy_config.as_deref().unwrap().display(),
            "(legacy)".yellow()
        );
    } else {
        println!(
            "{}: {}",
            "Config".yellow(),
            "not found (run 'octos init')".dimmed()
        );
    }

    // Workspace
    if data_dir.exists() {
        println!(
            "{}: {} {}",
            "Workspace".green(),
            data_dir.display(),
            "(found)".green()
        );
    } else {
        println!("{}: {}", "Workspace".yellow(), "not initialized".dimmed());
    }

    // Load config for provider/model info
    let config = Config::load_with_context(cwd, &ctx).unwrap_or_default();

    let provider = config.provider.as_deref().unwrap_or("(not configured)");
    let model = config.model.as_deref().unwrap_or("(not configured)");
    println!("{}: {}", "Provider".green(), provider);
    println!("{}: {}", "Model".green(), model);

    if let Some(ref url) = config.base_url {
        println!("{}: {}", "Base URL".green(), url);
    }

    // API keys
    println!();
    println!("{}", "API Keys".cyan().bold());
    println!("{}", "─".repeat(50).dimmed());

    for (label, env_var) in PROVIDER_ENV_VARS {
        // A provider may accept more than one key env var (e.g. Moonshot reads
        // MOONSHOT_API_KEY or KIMI_API_KEY via its "kimi" registry alias). Treat
        // it as "set" when the displayed var OR any alias-derived var is set, so
        // status matches what config resolution actually accepts.
        let mut is_set = std::env::var(env_var).is_ok();
        if !is_set {
            if let Some(entry) = octos_llm::registry::lookup(&label.to_lowercase()) {
                // Honor the provider's registry key var plus any declared sibling
                // key vars (e.g. MOONSHOT_API_KEY / KIMI_API_KEY), matching what
                // config resolution actually accepts.
                is_set = entry.api_key_env.is_some_and(|v| std::env::var(v).is_ok())
                    || entry
                        .key_env_aliases
                        .iter()
                        .any(|v| std::env::var(v).is_ok());
            }
        }
        let status = if is_set {
            "set".green().to_string()
        } else {
            "not set".dimmed().to_string()
        };
        println!("  {:<12} {:<24} {}", label, env_var.dimmed(), status);
    }

    // Bootstrap files
    println!();
    println!("{}", "Bootstrap Files".cyan().bold());
    println!("{}", "─".repeat(50).dimmed());

    for name in &["AGENTS.md", "SOUL.md", "USER.md", "TOOLS.md", "IDENTITY.md"] {
        let path = data_dir.join(name);
        let status = if path.exists() {
            "found".green().to_string()
        } else {
            "missing".dimmed().to_string()
        };
        println!("  {:<16} {}", name, status);
    }

    // Gateway config
    if let Some(ref gw) = config.gateway {
        println!();
        println!("{}", "Gateway".cyan().bold());
        println!("{}", "─".repeat(50).dimmed());
        println!(
            "  {}: {}",
            "Channels".dimmed(),
            gw.channels
                .iter()
                .map(|c| c.channel_type.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("  {}: {}", "Max history".dimmed(), gw.max_history);
    }

    println!();

    Ok(())
}
