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
    ("Moonshot", "MOONSHOT_API_KEY"),
    ("DashScope", "DASHSCOPE_API_KEY"),
    ("MiniMax", "MINIMAX_API_KEY"),
    ("Zhipu", "ZHIPU_API_KEY"),
];

fn show_system_status(cwd: &std::path::Path) -> Result<()> {
    println!("{}", "octos Status".cyan().bold());
    println!("{}", "═".repeat(50));
    println!();

    let config_path = cwd.join(".octos").join("config.json");
    let data_dir = super::resolve_data_dir(None)?;
    let data_dir_config = Config::data_dir_config_path(&data_dir);

    // Config location
    if config_path.exists() {
        println!(
            "{}: {} {}",
            "Config".green(),
            config_path.display(),
            "(found)".green()
        );
    } else if data_dir_config.exists() {
        println!(
            "{}: {} {}",
            "Config".green(),
            data_dir_config.display(),
            "(found)".green()
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
    let config = Config::load(cwd, &data_dir).unwrap_or_default();

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
        let status = if std::env::var(env_var).is_ok() {
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
