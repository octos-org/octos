//! Channels CLI subcommands for showing channel configuration.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::{Result, WrapErr};

use super::Executable;
use crate::config::Config;

/// Manage messaging channels.
#[derive(Debug, Args)]
pub struct ChannelsCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    #[command(subcommand)]
    pub subcommand: ChannelsSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ChannelsSubcommand {
    /// Show configured channels and their status.
    Status,
    /// Link WhatsApp device via QR code (requires Node.js bridge).
    Login {
        /// Path to the bridge directory (default: ./bridge).
        #[arg(long)]
        bridge_dir: Option<PathBuf>,
    },
}

impl Executable for ChannelsCommand {
    fn execute(self) -> Result<()> {
        let cwd = match self.cwd {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };

        match self.subcommand {
            ChannelsSubcommand::Status => cmd_status(&cwd),
            ChannelsSubcommand::Login { bridge_dir } => cmd_login(bridge_dir),
        }
    }
}

fn cmd_status(cwd: &std::path::Path) -> Result<()> {
    let config = Config::load(cwd)?;

    let channels = match &config.gateway {
        Some(gw) => &gw.channels,
        None => {
            println!("No gateway configuration found.");
            println!(
                "{}",
                "Add a 'gateway' section to .octos/config.json to enable channels.".dimmed()
            );
            return Ok(());
        }
    };

    if channels.is_empty() {
        println!("No channels configured.");
        return Ok(());
    }

    println!("{}", "Channel Status".cyan().bold());
    println!("{}", "=".repeat(60));
    println!();

    println!(
        "  {:<14} {:<12} {:<32}",
        "Channel".bold(),
        "Compiled".bold(),
        "Configuration".bold()
    );
    println!("  {}", "-".repeat(56));

    for entry in channels {
        let compiled = is_channel_compiled(&entry.channel_type);
        let compiled_str = if compiled {
            "yes".green().to_string()
        } else {
            "no".red().to_string()
        };

        let config_info = channel_config_summary(&entry.channel_type, &entry.settings);

        let senders = if entry.allowed_senders.is_empty() {
            String::new()
        } else {
            format!(" (allow: {})", entry.allowed_senders.join(", "))
        };

        println!(
            "  {:<14} {:<12} {}{}",
            entry.channel_type.cyan(),
            compiled_str,
            config_info,
            senders.dimmed(),
        );
    }

    println!();
    Ok(())
}

fn is_channel_compiled(channel_type: &str) -> bool {
    match channel_type {
        "cli" => true,
        #[cfg(feature = "telegram")]
        "telegram" => true,
        #[cfg(feature = "discord")]
        "discord" => true,
        #[cfg(feature = "slack")]
        "slack" => true,
        #[cfg(feature = "whatsapp")]
        "whatsapp" => true,
        #[cfg(feature = "feishu")]
        "feishu" | "lark" => true,
        #[cfg(feature = "wecom-bot")]
        "wecom-bot" => true,
        #[cfg(feature = "qq-bot")]
        "qq-bot" => true,
        _ => false,
    }
}

fn cmd_login(bridge_dir: Option<PathBuf>) -> Result<()> {
    let dir = bridge_dir.unwrap_or_else(|| PathBuf::from("bridge"));
    if !dir.exists() {
        eyre::bail!(
            "Bridge directory not found: {}\nClone or create a WhatsApp bridge (Baileys) at that path.",
            dir.display()
        );
    }

    println!("{}", "octos WhatsApp Login".cyan().bold());
    println!("Scan the QR code with your phone to connect.");
    println!();

    // Install npm deps if node_modules missing
    let node_modules = dir.join("node_modules");
    if !node_modules.exists() {
        println!("{}", "Installing bridge dependencies...".dimmed());
        let status = std::process::Command::new("npm")
            .arg("install")
            .current_dir(&dir)
            .status()
            .map_err(|_| eyre::eyre!("npm not found. Please install Node.js (20+)."))?;
        if !status.success() {
            eyre::bail!("npm install failed");
        }
    }

    // Launch bridge
    let status = std::process::Command::new("npm")
        .arg("start")
        .current_dir(&dir)
        .status()
        .map_err(|_| eyre::eyre!("npm not found. Please install Node.js (20+)."))?;

    if !status.success() {
        eyre::bail!("Bridge process exited with error");
    }

    Ok(())
}

fn channel_config_summary(channel_type: &str, settings: &serde_json::Value) -> String {
    match channel_type {
        "cli" => "built-in".into(),
        "telegram" => {
            let env = settings
                .get("token_env")
                .and_then(|v| v.as_str())
                .unwrap_or("TELEGRAM_BOT_TOKEN");
            let set = std::env::var(env).is_ok();
            if set {
                format!("{env}: set")
            } else {
                format!("{env}: not set")
            }
        }
        "discord" => {
            let env = settings
                .get("token_env")
                .and_then(|v| v.as_str())
                .unwrap_or("DISCORD_BOT_TOKEN");
            let set = std::env::var(env).is_ok();
            if set {
                format!("{env}: set")
            } else {
                format!("{env}: not set")
            }
        }
        "slack" => {
            let bot_env = settings
                .get("bot_token_env")
                .and_then(|v| v.as_str())
                .unwrap_or("SLACK_BOT_TOKEN");
            let app_env = settings
                .get("app_token_env")
                .and_then(|v| v.as_str())
                .unwrap_or("SLACK_APP_TOKEN");
            let bot_set = std::env::var(bot_env).is_ok();
            let app_set = std::env::var(app_env).is_ok();
            if bot_set && app_set {
                "socket mode: configured".into()
            } else {
                format!("{bot_env}/{app_env}: not set")
            }
        }
        "whatsapp" => {
            let url = settings
                .get("bridge_url")
                .and_then(|v| v.as_str())
                .unwrap_or("ws://localhost:3001");
            format!("bridge: {url}")
        }
        "feishu" | "lark" => {
            let id_env = settings
                .get("app_id_env")
                .and_then(|v| v.as_str())
                .unwrap_or("FEISHU_APP_ID");
            let set = std::env::var(id_env).is_ok();
            if set {
                format!("{id_env}: set")
            } else {
                format!("{id_env}: not set")
            }
        }
        other => format!("unknown: {other}"),
    }
}
