//! Init command: create config.json interactively in the resolved octos home.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use colored::Colorize;
use eyre::{Result, WrapErr, eyre};
use serde_json::{Value, json};

use super::Executable;

/// Known providers with their default env var and base URL.
/// Ordered by general popularity / accessibility.
struct ProviderInfo {
    name: &'static str,
    display: &'static str,
    api_key_env: &'static str,
    base_url: Option<&'static str>,
    api_type: Option<&'static str>,
    api_types: &'static [ApiTypeOption],
}

const CUSTOM_PROVIDER_NAME: &str = "custom";
const CUSTOM_API_KEY_ENV: &str = "CUSTOM_API_KEY";
const MODEL_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ApiTypeOption {
    value: Option<&'static str>,
    display: &'static str,
    description: &'static str,
    base_url: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ApiTypeSelection {
    api_type: Option<&'static str>,
    base_url: Option<&'static str>,
}

const MINIMAX_API_TYPES: &[ApiTypeOption] = &[
    ApiTypeOption {
        value: None,
        display: "OpenAI",
        description: "Chat Completions API",
        base_url: Some("https://api.minimax.io/v1"),
    },
    ApiTypeOption {
        value: Some("anthropic"),
        display: "Anthropic",
        description: "Messages API",
        base_url: Some("https://api.minimaxi.com/anthropic"),
    },
];

const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        name: "openai",
        display: "OpenAI (GPT-4o)",
        api_key_env: "OPENAI_API_KEY",
        base_url: None,
        api_type: None,
        api_types: &[],
    },
    ProviderInfo {
        name: "anthropic",
        display: "Anthropic (Claude)",
        api_key_env: "ANTHROPIC_API_KEY",
        base_url: None,
        api_type: None,
        api_types: &[],
    },
    ProviderInfo {
        name: "gemini",
        display: "Google Gemini",
        api_key_env: "GEMINI_API_KEY",
        base_url: None,
        api_type: None,
        api_types: &[],
    },
    ProviderInfo {
        name: "deepseek",
        display: "DeepSeek",
        api_key_env: "DEEPSEEK_API_KEY",
        base_url: Some("https://api.deepseek.com/v1"),
        api_type: None,
        api_types: &[],
    },
    ProviderInfo {
        name: "moonshot",
        display: "Moonshot (Kimi)",
        api_key_env: "KIMI_API_KEY",
        base_url: Some("https://api.moonshot.ai/v1"),
        api_type: None,
        api_types: &[],
    },
    ProviderInfo {
        name: "dashscope",
        display: "Dashscope (Qwen)",
        api_key_env: "DASHSCOPE_API_KEY",
        base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        api_type: None,
        api_types: &[],
    },
    ProviderInfo {
        name: "minimax",
        display: "MiniMax",
        api_key_env: "MINIMAX_API_KEY",
        base_url: Some("https://api.minimax.io/v1"),
        api_type: None,
        api_types: MINIMAX_API_TYPES,
    },
    ProviderInfo {
        name: "zai",
        display: "Z.AI (GLM)",
        api_key_env: "ZAI_API_KEY",
        base_url: Some("https://api.z.ai/api/anthropic"),
        api_type: Some("anthropic"),
        api_types: &[],
    },
];

fn default_api_type_selection(info: &ProviderInfo) -> ApiTypeSelection {
    ApiTypeSelection {
        api_type: info.api_type,
        base_url: info.base_url,
    }
}

fn select_api_type(info: &ProviderInfo, raw_input: &str) -> ApiTypeSelection {
    if info.api_types.is_empty() {
        return default_api_type_selection(info);
    }

    let selected = if raw_input.trim().is_empty() {
        0
    } else {
        match raw_input.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= info.api_types.len() => n - 1,
            _ => 0,
        }
    };

    let option = info.api_types[selected];
    ApiTypeSelection {
        api_type: option.value,
        base_url: option.base_url.or(info.base_url),
    }
}

fn build_config(
    info: &ProviderInfo,
    model: &str,
    api_key_env: &str,
    api_selection: ApiTypeSelection,
) -> serde_json::Value {
    let mut config = json!({
        "provider": info.name,
        "model": model,
        "api_key_env": api_key_env
    });

    if let Some(base_url) = api_selection.base_url {
        config["base_url"] = json!(base_url);
    }
    if let Some(api_type) = api_selection.api_type {
        config["api_type"] = json!(api_type);
    }

    config
}

/// A fully-resolved custom (self-hosted / proxy) provider selection.
///
/// The preset providers in `PROVIDERS` are written via [`build_config`] which
/// keys off a [`ProviderInfo`]; the custom path has no static `ProviderInfo`,
/// so it carries its own owned fields here and serializes via [`Self::to_json`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedProvider {
    provider: String,
    model: String,
    api_key_env: String,
    base_url: Option<String>,
    api_type: Option<String>,
}

impl SelectedProvider {
    fn to_json(&self) -> Value {
        let mut config = json!({
            "provider": self.provider,
            "model": self.model,
            "api_key_env": self.api_key_env
        });
        if let Some(base_url) = &self.base_url {
            config["base_url"] = json!(base_url);
        }
        if let Some(api_type) = &self.api_type {
            config["api_type"] = json!(api_type);
        }
        config
    }
}

fn validate_base_url(input: &str) -> Result<String> {
    let trimmed = input.trim().trim_end_matches('/').to_string();
    let parsed = reqwest::Url::parse(&trimmed)
        .wrap_err_with(|| format!("base_url '{trimmed}' is not a valid URL"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(trimmed),
        other => eyre::bail!("base_url scheme '{other}' is not supported; use http or https"),
    }
}

fn models_endpoint(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

fn parse_model_ids(value: &Value) -> Vec<String> {
    if let Some(data) = value.get("data").and_then(Value::as_array) {
        return data
            .iter()
            .filter_map(|item| item.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
    }

    value
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.as_str()
                .or_else(|| item.get("id").and_then(Value::as_str))
        })
        .map(str::to_string)
        .collect()
}

fn fetch_custom_models(base_url: &str, api_key_env: &str) -> Result<Vec<String>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(MODEL_FETCH_TIMEOUT)
        .build()?;
    let mut request = client
        .get(models_endpoint(base_url))
        .header(reqwest::header::ACCEPT, "application/json");

    if let Ok(api_key) = std::env::var(api_key_env) {
        let api_key = api_key.trim();
        if !api_key.is_empty() {
            request = request.bearer_auth(api_key);
        }
    }

    let response = request.send()?.error_for_status()?;
    let body: Value = response.json()?;
    Ok(parse_model_ids(&body))
}

fn prompt_api_type() -> Result<String> {
    println!();
    println!("Available API types:");
    println!("  1. OpenAI (Chat Completions API)");
    println!("  2. Anthropic (Messages API)");
    print!("Select API type [1]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(match input.trim() {
        "" | "1" | "openai" | "OpenAI" => "openai".to_string(),
        "2" | "anthropic" | "Anthropic" => "anthropic".to_string(),
        _ => {
            println!("{}", "Invalid API type, using OpenAI".yellow());
            "openai".to_string()
        }
    })
}

fn prompt_model(default_model: &str, fetched_models: &[String]) -> Result<String> {
    println!();
    if fetched_models.is_empty() {
        print!("Custom Model Name [{default_model}]: ");
    } else {
        println!("Available models:");
        for (i, model) in fetched_models.iter().enumerate() {
            let rec = if i == 0 { " (default)" } else { "" };
            println!("  {}. {}{}", i + 1, model, rec);
        }
        print!("Select model or enter custom name [1]: ");
    }
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(fetched_models
            .first()
            .cloned()
            .unwrap_or_else(|| default_model.to_string()));
    }
    if let Ok(choice) = trimmed.parse::<usize>() {
        if choice >= 1 && choice <= fetched_models.len() {
            return Ok(fetched_models[choice - 1].clone());
        }
    }
    Ok(trimmed.to_string())
}

fn prompt_custom_provider() -> Result<SelectedProvider> {
    println!();
    println!("{}", "Custom provider configuration".green());

    let base_url = loop {
        print!("Custom API Base URL: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match validate_base_url(&input) {
            Ok(url) => break url,
            Err(err) => println!("{} {err:#}", "Invalid base URL:".yellow()),
        }
    };

    let api_type = prompt_api_type()?;

    println!();
    print!("Environment variable containing the API Key [{CUSTOM_API_KEY_ENV}]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let api_key_env = if input.trim().is_empty() {
        CUSTOM_API_KEY_ENV.to_string()
    } else {
        input.trim().to_string()
    };

    println!();
    print!(
        "Fetching available models from {} ... ",
        models_endpoint(&base_url)
    );
    io::stdout().flush()?;
    let fetched_models = match fetch_custom_models(&base_url, &api_key_env) {
        Ok(models) if !models.is_empty() => {
            println!("{}", "✓".green());
            models
        }
        Ok(_) => {
            println!("{}", "no models returned".yellow());
            Vec::new()
        }
        Err(err) => {
            println!("{}", "unavailable".yellow());
            println!("  {}", format!("{err:#}").dimmed());
            Vec::new()
        }
    };

    let model = prompt_model("auto", &fetched_models)?;

    Ok(SelectedProvider {
        provider: CUSTOM_PROVIDER_NAME.to_string(),
        model,
        api_key_env,
        base_url: Some(base_url),
        api_type: Some(api_type),
    })
}

/// Write the resolved config plus the bootstrap scaffolding (gitignore,
/// AGENTS.md/SOUL.md/USER.md templates, subdirectories, final hints).
///
/// Shared by the preset path (config built via [`build_config`]) and the
/// custom path (config built via [`SelectedProvider::to_json`]) so both flows
/// produce an identical, fully-bootstrapped octos home.
fn write_init_files(
    config_dir: PathBuf,
    config_path: PathBuf,
    config: Value,
    api_key_env: String,
) -> Result<()> {
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
            "# Soul — Who You Are\n\n\
             ## Core Principles\n\n\
             - **Help, don't perform.** Skip filler phrases. No \"Great question!\" or \"I'd be happy to help!\" — just do the thing.\n\
             - **Be resourceful.** Read the file. Check context. Search for it. Come back with answers, not questions.\n\
             - **Have a voice.** You can disagree, suggest alternatives, flag bad ideas. A useful assistant has opinions.\n\
             - **Match the medium.** Telegram gets concise replies. CLI gets detail. Email gets structure. Read the room.\n\n\
             ## Trust & Safety\n\n\
             - You're a guest in someone's digital life. Act like it.\n\
             - Private things stay private. No leaking context across sessions or users.\n\
             - **External actions need care.** Sending messages, emails, or making API calls — double-check before acting.\n\
             - **Internal actions are yours.** Reading files, searching, organizing, running sandboxed commands — be bold.\n\
             - Never send half-finished replies to messaging channels.\n\n\
             ## Working Style\n\n\
             - Prefer doing over explaining what you'll do.\n\
             - When a task is ambiguous, make a reasonable choice and state your assumption.\n\
             - Use tools. You have them for a reason.\n\
             - If something fails, diagnose before retrying.\n\
             - Keep responses proportional to the question.\n\n\
             ## Continuity\n\n\
             Your memory persists through episodes and MEMORY.md.\n\
             Bootstrap files (this one included) are loaded every session.\n\
             If you update this file, tell the user — it defines who you are.\n",
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
    println!("{}", "Ready! Run 'octos chat' to start.".green().bold());

    Ok(())
}

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
        std::env::current_exe().ok().and_then(|p| {
            p.parent()?
                .parent()?
                .parent()
                .map(|d| d.join("model_catalog.json"))
        }),
        // Current directory
        Some(PathBuf::from("model_catalog.json")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if let Ok(content) = std::fs::read_to_string(&candidate) {
            if let Ok(catalog) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(models) = catalog.get("models").and_then(|m| m.as_array()) {
                    for model in models {
                        if let Some(provider_model) = model.get("provider").and_then(|p| p.as_str())
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

/// Initialize a new octos configuration.
#[derive(Debug, Args)]
pub struct InitCommand {
    /// Project working directory. When set, init writes to `<cwd>/.octos/`.
    /// Otherwise it writes to `$OCTOS_HOME` or `~/.octos`.
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Skip interactive prompts and use defaults.
    #[arg(long)]
    pub defaults: bool,

    /// Overwrite an existing config without prompting.
    #[arg(long)]
    pub force: bool,

    /// Configure a custom/self-hosted OpenAI- or Anthropic-compatible API base URL.
    #[arg(long)]
    pub custom_base_url: Option<String>,

    /// Model name to write with --custom-base-url.
    #[arg(long)]
    pub custom_model: Option<String>,

    /// API protocol to write with --custom-base-url: openai or anthropic.
    #[arg(long, value_parser = ["openai", "anthropic"])]
    pub custom_api_type: Option<String>,

    /// Environment variable containing the API key for --custom-base-url.
    #[arg(long)]
    pub custom_api_key_env: Option<String>,
}

impl Executable for InitCommand {
    fn execute(self) -> Result<()> {
        println!("{}", "octos init".cyan().bold());
        println!();

        // Resolve a flag-driven custom provider up front so an invalid
        // --custom-* combination fails before we touch the filesystem.
        let custom_flag_selection = self.custom_selection_from_flags()?;

        // Where to write the starter config:
        //   --cwd C        → C/.octos (explicit project-local request)
        //   otherwise      → the resolver's config_home (OCTOS_CONFIG_DIR if
        //                    set; the state dir for an explicit OCTOS_HOME /
        //                    --data-dir; else the XDG default for a default
        //                    install).
        let config_dir = match self.cwd {
            Some(cwd) => cwd.join(".octos"),
            None => {
                let ctx = super::resolve_command_context(None)?;
                ctx.config_home
            }
        };
        let config_path = config_dir.join("config.json");

        // Check if config already exists
        if config_path.exists() {
            println!(
                "{} {}",
                "Config already exists:".yellow(),
                config_path.display()
            );
            if self.defaults && !self.force {
                return Err(eyre!(
                    "Config already exists at {}. Re-run with --force to overwrite it.",
                    config_path.display()
                ));
            }
            if !self.defaults && !self.force {
                print!("Overwrite? [y/N] ");
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    println!("Aborted.");
                    return Ok(());
                }
            }
            if self.force {
                println!(
                    "{}",
                    "Overwriting existing config because --force was set.".yellow()
                );
            }
        }

        // Flag-driven custom provider: write it directly and skip the preset
        // selection flow entirely.
        if let Some(custom) = custom_flag_selection {
            let api_key_env = custom.api_key_env.clone();
            return write_init_files(config_dir, config_path, custom.to_json(), api_key_env);
        }

        // Load model catalog for hints
        let catalog = load_catalog_models();

        let (provider_info_idx, model, api_key_env, api_selection) = if self.defaults {
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
            (
                idx,
                default_model,
                info.api_key_env.to_string(),
                default_api_type_selection(info),
            )
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
            println!("    {}. Custom (self-hosted or proxy)", PROVIDERS.len() + 1);
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
                    Ok(n) if n == PROVIDERS.len() + 1 => {
                        let custom = prompt_custom_provider()?;
                        let api_key_env = custom.api_key_env.clone();
                        return write_init_files(
                            config_dir,
                            config_path,
                            custom.to_json(),
                            api_key_env,
                        );
                    }
                    _ => {
                        println!("{}", "Invalid selection, using detected/default".yellow());
                        default_idx
                    }
                }
            };

            let info = &PROVIDERS[idx];

            let api_selection = if info.api_types.is_empty() {
                default_api_type_selection(info)
            } else {
                println!();
                println!("Available API types for {}:", info.display);
                for (i, option) in info.api_types.iter().enumerate() {
                    let default = if i == 0 { " - default" } else { "" };
                    println!(
                        "  {}. {} ({}){}",
                        i + 1,
                        option.display,
                        option.description,
                        default
                    );
                }
                println!();
                print!("Select API type [1]: ");
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                let selection = select_api_type(info, &input);
                if let Some(api_type) = selection.api_type {
                    println!(
                        "Selected API type: {}",
                        api_type.to_ascii_uppercase().bold()
                    );
                } else {
                    println!("Selected API type: {}", "OPENAI".bold());
                }
                if let Some(base_url) = selection.base_url {
                    println!("Base URL: {base_url}");
                }
                selection
            };

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
            print!(
                "Environment variable containing the API Key [{}]: ",
                info.api_key_env
            );
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let api_key_env = if input.trim().is_empty() {
                info.api_key_env.to_string()
            } else {
                input.trim().to_string()
            };

            (idx, model, api_key_env, api_selection)
        };

        let info = &PROVIDERS[provider_info_idx];

        let config = build_config(info, &model, &api_key_env, api_selection);

        write_init_files(config_dir, config_path, config, api_key_env)
    }
}

impl InitCommand {
    /// Build a [`SelectedProvider`] from the `--custom-*` flags, or `None` when
    /// no custom flag was passed. `--custom-base-url` is required whenever any
    /// custom flag is present.
    fn custom_selection_from_flags(&self) -> Result<Option<SelectedProvider>> {
        let has_custom_flag = self.custom_base_url.is_some()
            || self.custom_model.is_some()
            || self.custom_api_type.is_some()
            || self.custom_api_key_env.is_some();
        if !has_custom_flag {
            return Ok(None);
        }

        let Some(base_url) = &self.custom_base_url else {
            eyre::bail!("--custom-base-url is required when using custom init flags");
        };

        Ok(Some(SelectedProvider {
            provider: CUSTOM_PROVIDER_NAME.to_string(),
            model: self
                .custom_model
                .clone()
                .unwrap_or_else(|| "auto".to_string()),
            api_key_env: self
                .custom_api_key_env
                .clone()
                .unwrap_or_else(|| CUSTOM_API_KEY_ENV.to_string()),
            base_url: Some(validate_base_url(base_url)?),
            api_type: Some(
                self.custom_api_type
                    .clone()
                    .unwrap_or_else(|| "openai".to_string()),
            ),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str) -> &'static ProviderInfo {
        PROVIDERS
            .iter()
            .find(|provider| provider.name == name)
            .unwrap_or_else(|| panic!("missing provider: {name}"))
    }

    #[test]
    fn minimax_anthropic_api_type_switches_base_url() {
        let selection = select_api_type(provider("minimax"), "2");

        assert_eq!(selection.api_type, Some("anthropic"));
        assert_eq!(
            selection.base_url,
            Some("https://api.minimaxi.com/anthropic")
        );
    }

    #[test]
    fn minimax_blank_or_invalid_api_type_uses_openai_compatible_default() {
        for input in ["", "0", "99", "anthropic"] {
            let selection = select_api_type(provider("minimax"), input);

            assert_eq!(selection.api_type, None);
            assert_eq!(selection.base_url, Some("https://api.minimax.io/v1"));
        }
    }

    #[test]
    fn zai_default_selection_preserves_anthropic_api_type_and_base_url() {
        let selection = default_api_type_selection(provider("zai"));

        assert_eq!(selection.api_type, Some("anthropic"));
        assert_eq!(selection.base_url, Some("https://api.z.ai/api/anthropic"));
    }

    #[test]
    fn config_includes_api_type_when_selected() {
        let config = build_config(
            provider("minimax"),
            "MiniMax-M2.7",
            "MINIMAX_API_KEY",
            select_api_type(provider("minimax"), "2"),
        );

        assert_eq!(config["provider"], "minimax");
        assert_eq!(config["model"], "MiniMax-M2.7");
        assert_eq!(config["api_key_env"], "MINIMAX_API_KEY");
        assert_eq!(config["api_type"], "anthropic");
        assert_eq!(config["base_url"], "https://api.minimaxi.com/anthropic");
    }

    #[test]
    fn config_omits_api_type_for_openai_compatible_default() {
        let config = build_config(
            provider("minimax"),
            "MiniMax-M2.7",
            "MINIMAX_API_KEY",
            select_api_type(provider("minimax"), ""),
        );

        assert_eq!(config["provider"], "minimax");
        assert_eq!(config["base_url"], "https://api.minimax.io/v1");
        assert!(config.get("api_type").is_none());
    }

    #[test]
    fn custom_provider_config_writes_required_fields() {
        let selected = SelectedProvider {
            provider: CUSTOM_PROVIDER_NAME.to_string(),
            model: "llama-3.1-70b-instruct".to_string(),
            api_key_env: CUSTOM_API_KEY_ENV.to_string(),
            base_url: Some("https://api.example.com/v1".to_string()),
            api_type: Some("openai".to_string()),
        };

        let config = selected.to_json();

        assert_eq!(config["provider"], json!("custom"));
        assert_eq!(config["model"], json!("llama-3.1-70b-instruct"));
        assert_eq!(config["base_url"], json!("https://api.example.com/v1"));
        assert_eq!(config["api_type"], json!("openai"));
        assert_eq!(config["api_key_env"], json!("CUSTOM_API_KEY"));
    }

    #[test]
    fn custom_flags_build_custom_selection() {
        let cmd = InitCommand {
            cwd: None,
            defaults: false,
            force: false,
            custom_base_url: Some("https://api.example.com/v1/".to_string()),
            custom_model: Some("qwen-2.5-14b".to_string()),
            custom_api_type: Some("anthropic".to_string()),
            custom_api_key_env: Some("EXAMPLE_KEY".to_string()),
        };

        let selected = cmd.custom_selection_from_flags().unwrap().unwrap();

        assert_eq!(selected.provider, "custom");
        assert_eq!(selected.model, "qwen-2.5-14b");
        assert_eq!(selected.api_key_env, "EXAMPLE_KEY");
        assert_eq!(
            selected.base_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        assert_eq!(selected.api_type.as_deref(), Some("anthropic"));
    }

    #[test]
    fn custom_flags_require_base_url() {
        let cmd = InitCommand {
            cwd: None,
            defaults: false,
            force: false,
            custom_base_url: None,
            custom_model: Some("qwen-2.5-14b".to_string()),
            custom_api_type: None,
            custom_api_key_env: None,
        };

        let err = cmd.custom_selection_from_flags().unwrap_err();

        assert!(err.to_string().contains("--custom-base-url is required"));
    }

    #[test]
    fn custom_models_endpoint_trims_trailing_slashes() {
        assert_eq!(
            models_endpoint("https://api.example.com/v1/"),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn parses_openai_models_response() {
        let body = json!({
            "data": [
                {"id": "llama-3.1-70b-instruct"},
                {"id": "qwen-2.5-14b"}
            ]
        });

        assert_eq!(
            parse_model_ids(&body),
            vec![
                "llama-3.1-70b-instruct".to_string(),
                "qwen-2.5-14b".to_string()
            ]
        );
    }

    #[test]
    fn parses_plain_models_array_response() {
        let body = json!({
            "models": [
                "mistral-large",
                {"id": "codestral"}
            ]
        });

        assert_eq!(
            parse_model_ids(&body),
            vec!["mistral-large".to_string(), "codestral".to_string()]
        );
    }
}
