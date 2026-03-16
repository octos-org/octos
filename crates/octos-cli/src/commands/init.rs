//! Init command: create .octos/config.json interactively.

use std::io::{self, Write};
use std::path::PathBuf;

use clap::Args;
use colored::Colorize;
use eyre::{Result, WrapErr};
use serde_json::json;

use super::Executable;

/// Initialize a new .octos configuration.
#[derive(Debug, Args)]
pub struct InitCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Skip interactive prompts and use defaults.
    #[arg(long)]
    pub defaults: bool,
}

impl Executable for InitCommand {
    fn execute(self) -> Result<()> {
        println!("{}", "octos init".cyan().bold());
        println!();

        let cwd = match self.cwd {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };

        let config_dir = cwd.join(".octos");
        let config_path = config_dir.join("config.json");

        // Check if config already exists
        if config_path.exists() {
            println!(
                "{} {}",
                "Config already exists:".yellow(),
                config_path.display()
            );
            if !self.defaults {
                print!("Overwrite? [y/N] ");
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    println!("Aborted.");
                    return Ok(());
                }
            }
        }

        let (provider, model, api_key_env) = if self.defaults {
            (
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
                "ANTHROPIC_API_KEY".to_string(),
            )
        } else {
            // Interactive prompts
            println!("{}", "Configure your LLM provider".green());
            println!();

            // Provider selection
            println!("Available providers:");
            println!("  1. anthropic (Claude)");
            println!("  2. openai (GPT-4)");
            println!("  3. gemini (Google Gemini)");
            println!("  4. zhipu (GLM)");
            println!("  5. deepseek (DeepSeek)");
            println!("  6. dashscope (Qwen)");
            println!("  7. moonshot (Kimi)");
            println!();
            print!("Select provider [1]: ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let provider = match input.trim() {
                "" | "1" => "anthropic",
                "2" => "openai",
                "3" => "gemini",
                "4" => "zhipu",
                "5" => "deepseek",
                "6" => "dashscope",
                "7" => "moonshot",
                _ => {
                    println!("{}", "Invalid selection, using anthropic".yellow());
                    "anthropic"
                }
            };

            // Model selection
            let default_model = match provider {
                "anthropic" => "claude-sonnet-4-20250514",
                "openai" => "gpt-4o",
                "gemini" => "gemini-2.0-flash",
                "zhipu" => "glm-4.7",
                "deepseek" => "deepseek-chat",
                "dashscope" => "qwen-max",
                "moonshot" => "kimi-k2.5",
                _ => "claude-sonnet-4-20250514",
            };

            println!();
            println!("Available models for {}:", provider);
            match provider {
                "anthropic" => {
                    println!("  - claude-sonnet-4-20250514 (recommended)");
                    println!("  - claude-opus-4-20250514");
                    println!("  - claude-3-5-haiku-20241022");
                }
                "openai" => {
                    println!("  - gpt-4o (recommended)");
                    println!("  - gpt-4o-mini");
                    println!("  - gpt-4-turbo");
                }
                "gemini" => {
                    println!("  - gemini-2.0-flash (recommended)");
                    println!("  - gemini-2.0-flash-lite");
                    println!("  - gemini-1.5-pro");
                }
                "zhipu" => {
                    println!("  - glm-4.7 (recommended)");
                    println!("  - glm-4.5");
                    println!("  - glm-4-flash");
                }
                "deepseek" => {
                    println!("  - deepseek-chat (recommended)");
                    println!("  - deepseek-reasoner");
                }
                "dashscope" => {
                    println!("  - qwen-max (recommended)");
                    println!("  - qwen-turbo");
                    println!("  - qwq-plus");
                }
                "moonshot" => {
                    println!("  - kimi-k2.5 (recommended)");
                }
                _ => {}
            }
            println!();
            print!("Model [{}]: ", default_model);
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let model = if input.trim().is_empty() {
                default_model.to_string()
            } else {
                input.trim().to_string()
            };

            // API key env var
            let default_env = match provider {
                "anthropic" => "ANTHROPIC_API_KEY",
                "openai" => "OPENAI_API_KEY",
                "gemini" => "GEMINI_API_KEY",
                "zhipu" => "ZHIPU_API_KEY",
                "deepseek" => "DEEPSEEK_API_KEY",
                "dashscope" => "DASHSCOPE_API_KEY",
                "moonshot" => "MOONSHOT_API_KEY",
                _ => "API_KEY",
            };

            println!();
            print!("API key environment variable [{}]: ", default_env);
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let api_key_env = if input.trim().is_empty() {
                default_env.to_string()
            } else {
                input.trim().to_string()
            };

            (provider.to_string(), model, api_key_env)
        };

        // Create config
        let config = json!({
            "provider": provider,
            "model": model,
            "api_key_env": api_key_env
        });

        // Create directory
        std::fs::create_dir_all(&config_dir)
            .wrap_err_with(|| format!("failed to create directory: {}", config_dir.display()))?;

        // Write config
        let config_str = serde_json::to_string_pretty(&config)?;
        std::fs::write(&config_path, &config_str)
            .wrap_err_with(|| format!("failed to write config: {}", config_path.display()))?;

        println!();
        println!("{}", "─".repeat(50).dimmed());
        println!();
        println!("{} {}", "Created:".green(), config_path.display());
        println!();
        println!("{}", "Config:".cyan());
        println!("{}", config_str);
        println!();

        // Check if API key is set
        if std::env::var(&api_key_env).is_err() {
            println!("{} {} is not set", "Warning:".yellow(), api_key_env);
            println!();
            println!("Set it with:");
            println!("  export {}=your-api-key", api_key_env);
            println!();
        } else {
            println!("{} {} is set", "✓".green(), api_key_env);
        }

        // Create .gitignore if it doesn't exist
        let gitignore_path = config_dir.join(".gitignore");
        if !gitignore_path.exists() {
            std::fs::write(
                &gitignore_path,
                "# Ignore task state and database files\ntasks/\nsessions/\n*.redb\n",
            )?;
            println!("{} {}", "Created:".green(), gitignore_path.display());
        }

        // Create bootstrap template files (skip existing)
        let templates: &[(&str, &str)] = &[
            (
                "AGENTS.md",
                "# Agent Instructions\n\nCustomize agent behavior and guidelines here.\n",
            ),
            (
                "SOUL.md",
                "# Personality\n\nDefine the agent's personality and values.\n",
            ),
            (
                "USER.md",
                "# User Info\n\nAdd your information and preferences here.\n",
            ),
        ];

        for (name, content) in templates {
            let path = config_dir.join(name);
            if !path.exists() {
                std::fs::write(&path, content)?;
                println!("{} {}", "Created:".green(), path.display());
            }
        }

        // Create subdirectories
        for dir in &["memory", "sessions", "skills"] {
            let path = config_dir.join(dir);
            if !path.exists() {
                std::fs::create_dir_all(&path)?;
                println!("{} {}/", "Created:".green(), path.display());
            }
        }

        println!();
        println!(
            "{}",
            "Ready! Run 'octos run <goal>' or 'octos chat' to start."
                .green()
                .bold()
        );

        Ok(())
    }
}
