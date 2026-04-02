//! Init command: create .octos/config.json interactively.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;

use clap::Args;
use colored::Colorize;
use eyre::{Result, WrapErr};
use serde_json::json;

use super::Executable;

/// Known providers with their default env var and base URL.
/// Ordered by general popularity / accessibility.
struct ProviderInfo {
    name: &'static str,
    display: &'static str,
    api_key_env: &'static str,
    base_url: Option<&'static str>,
    api_type: Option<&'static str>,
}

const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        name: "openai",
        display: "OpenAI (GPT-4o)",
        api_key_env: "OPENAI_API_KEY",
        base_url: None,
        api_type: None,
    },
    ProviderInfo {
        name: "anthropic",
        display: "Anthropic (Claude)",
        api_key_env: "ANTHROPIC_API_KEY",
        base_url: None,
        api_type: None,
    },
    ProviderInfo {
        name: "gemini",
        display: "Google Gemini",
        api_key_env: "GEMINI_API_KEY",
        base_url: None,
        api_type: None,
    },
    ProviderInfo {
        name: "deepseek",
        display: "DeepSeek",
        api_key_env: "DEEPSEEK_API_KEY",
        base_url: Some("https://api.deepseek.com/v1"),
        api_type: None,
    },
    ProviderInfo {
        name: "moonshot",
        display: "Moonshot (Kimi)",
        api_key_env: "KIMI_API_KEY",
        base_url: Some("https://api.moonshot.ai/v1"),
        api_type: None,
    },
    ProviderInfo {
        name: "dashscope",
        display: "Dashscope (Qwen)",
        api_key_env: "DASHSCOPE_API_KEY",
        base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        api_type: None,
    },
    ProviderInfo {
        name: "minimax",
        display: "MiniMax",
        api_key_env: "MINIMAX_API_KEY",
        base_url: Some("https://api.minimax.io/v1"),
        api_type: None,
    },
    ProviderInfo {
        name: "zai",
        display: "Z.AI (GLM)",
        api_key_env: "ZAI_API_KEY",
        base_url: Some("https://api.z.ai/api/anthropic"),
        api_type: Some("anthropic"),
    },
];

/// Load models from model_catalog.json, grouped by provider.
fn load_catalog_models() -> BTreeMap<String, Vec<String>> {
    let mut result = BTreeMap::new();

    // Try common locations for model_catalog.json
    let candidates = [
        // Next to the binary
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("model_catalog.json"))),
        // Workspace root (for development)
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent()?.parent()?.parent().map(|d| d.join("model_catalog.json"))),
        // Current directory
        Some(PathBuf::from("model_catalog.json")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if let Ok(content) = std::fs::read_to_string(&candidate) {
            if let Ok(catalog) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(models) = catalog.get("models").and_then(|m| m.as_array()) {
                    for model in models {
                        if let Some(provider_model) =
                            model.get("provider").and_then(|p| p.as_str())
                        {
                            let parts: Vec<&str> = provider_model.splitn(2, '/').collect();
                            if parts.len() == 2 {
                                result
                                    .entry(parts[0].to_string())
                                    .or_insert_with(Vec::new)
                                    .push(parts[1].to_string());
                            }
                        }
                    }
                    break;
                }
            }
        }
    }

    result
}

/// Auto-detect provider from available environment variables.
fn detect_from_env() -> Option<usize> {
    for (i, p) in PROVIDERS.iter().enumerate() {
        if std::env::var(p.api_key_env).is_ok() {
            return Some(i);
        }
    }
    None
}

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

        // Load model catalog for hints
        let catalog = load_catalog_models();

        let (provider_info_idx, model, api_key_env) = if self.defaults {
            // Auto-detect from env vars, or prompt if none found
            let idx = detect_from_env().unwrap_or(0); // fallback to first (openai)
            let info = &PROVIDERS[idx];
            let default_model = catalog
                .get(info.name)
                .and_then(|m| m.first().cloned())
                .unwrap_or_else(|| match info.name {
                    "openai" => "gpt-4.1-mini".to_string(),
                    "anthropic" => "claude-sonnet-4-20250514".to_string(),
                    _ => "auto".to_string(),
                });
            (idx, default_model, info.api_key_env.to_string())
        } else {
            // Interactive prompts
            println!("{}", "Configure your LLM provider".green());
            println!();

            // Show auto-detected provider if any
            if let Some(detected) = detect_from_env() {
                println!(
                    "  {} {} detected ({})",
                    "✓".green(),
                    PROVIDERS[detected].display,
                    PROVIDERS[detected].api_key_env
                );
                println!();
            }

            // Provider selection
            println!("Available providers:");
            for (i, p) in PROVIDERS.iter().enumerate() {
                let env_set = std::env::var(p.api_key_env).is_ok();
                let marker = if env_set {
                    "✓".green().to_string()
                } else {
                    " ".to_string()
                };
                println!("  {marker} {}. {}", i + 1, p.display);
            }
            println!();

            let default_idx = detect_from_env().unwrap_or(0);
            print!("Select provider [{}]: ", default_idx + 1);
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let idx = if input.trim().is_empty() {
                default_idx
            } else {
                match input.trim().parse::<usize>() {
                    Ok(n) if n >= 1 && n <= PROVIDERS.len() => n - 1,
                    _ => {
                        println!("{}", "Invalid selection, using detected/default".yellow());
                        default_idx
                    }
                }
            };

            let info = &PROVIDERS[idx];

            // Model selection — show from catalog if available
            let catalog_models = catalog.get(info.name);
            let default_model = catalog_models
                .and_then(|m| m.first().cloned())
                .unwrap_or_else(|| "auto".to_string());

            println!();
            if let Some(models) = catalog_models {
                println!("Available models for {} (from catalog):", info.display);
                for (i, m) in models.iter().enumerate() {
                    let rec = if i == 0 { " (recommended)" } else { "" };
                    println!("  - {}{}", m, rec);
                }
            } else {
                println!(
                    "No catalog models found for {}. Enter model name manually:",
                    info.display
                );
            }
            println!();
            print!("Model [{}]: ", default_model);
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let model = if input.trim().is_empty() {
                default_model
            } else {
                input.trim().to_string()
            };

            // API key env var
            println!();
            print!("API key environment variable [{}]: ", info.api_key_env);
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let api_key_env = if input.trim().is_empty() {
                info.api_key_env.to_string()
            } else {
                input.trim().to_string()
            };

            (idx, model, api_key_env)
        };

        let info = &PROVIDERS[provider_info_idx];

        // Create config
        let mut config = json!({
            "provider": info.name,
            "model": model,
            "api_key_env": api_key_env
        });

        // Add base_url for providers that need it
        if let Some(base_url) = info.base_url {
            config["base_url"] = json!(base_url);
        }
        // Add api_type for non-OpenAI protocol providers
        if let Some(api_type) = info.api_type {
            config["api_type"] = json!(api_type);
        }

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
            "Ready! Run 'octos chat' to start.".green().bold()
        );

        Ok(())
    }
}
