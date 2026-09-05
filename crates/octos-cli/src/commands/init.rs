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
        description: "Messages API (required for sk-cp- Coding-plan keys)",
        base_url: Some("https://api.minimaxi.com/anthropic"),
    },
];

const MINIMAX_CN_API_TYPES: &[ApiTypeOption] = &[
    ApiTypeOption {
        value: None,
        display: "OpenAI",
        description: "Chat Completions API",
        base_url: Some("https://api.minimaxi.com/v1"),
    },
    ApiTypeOption {
        value: Some("anthropic"),
        display: "Anthropic",
        description: "Messages API (required for sk-cp- Coding-plan keys)",
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
    // Region variant of the `minimax` preset: MiniMax Token-plan keys are
    // issued by the China platform and 401 against the international site
    // (octos#2125). Appended last so every existing preset index keeps its
    // number; only the trailing Custom entry (always `PROVIDERS.len() + 1`)
    // moves.
    ProviderInfo {
        name: "minimax-cn",
        display: "MiniMax China (Token Plan)",
        api_key_env: "MINIMAX_CN_API_KEY",
        base_url: Some("https://api.minimaxi.com/v1"),
        api_type: None,
        api_types: MINIMAX_CN_API_TYPES,
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

    let model = if fetched_models.is_empty() {
        // The /models probe found nothing — require an explicit name
        // instead of the old invalid "auto" default (#1541 item 3).
        let mut stdin = io::stdin().lock();
        read_required_model(&mut stdin, "the custom provider")?
    } else {
        prompt_model(&fetched_models[0].clone(), &fetched_models)?
    };

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
/// Outcome of the init-time API-key capture offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyCaptureOutcome {
    /// The user pasted a key; it was saved to the global auth store.
    Saved,
    /// The user pressed Enter to skip.
    Skipped,
}

/// Core of the init-time key capture: read one line from `reader`; empty
/// input skips, anything else is trimmed and saved to `store` as a
/// `paste_token` credential for `provider` — byte-for-byte the same
/// credential shape `octos auth login --provider <name>` stores, so
/// `Config::get_api_key` (auth store first, env second) resolves it for
/// chat/serve/gateway without any exported env var.
///
/// Extracted from the interactive wrapper for testability (injected reader).
fn capture_api_key_from_reader(
    provider: &str,
    store: &mut crate::auth::AuthStore,
    reader: &mut dyn std::io::BufRead,
) -> Result<KeyCaptureOutcome> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let token = line.trim();
    if token.is_empty() {
        return Ok(KeyCaptureOutcome::Skipped);
    }
    store.set(
        provider,
        crate::auth::AuthCredential {
            access_token: token.to_string(),
            refresh_token: None,
            expires_at: None,
            provider: provider.to_string(),
            auth_method: "paste_token".to_string(),
        },
    )?;
    Ok(KeyCaptureOutcome::Saved)
}

/// Preflight decision for the init-time credential offer, mirroring the
/// runtime resolution order in `Config::get_api_key`: a set env var or a
/// non-expired stored credential means the setup already works; anything
/// else (nothing stored, or only an EXPIRED credential — which
/// `get_api_key` rejects) warrants the capture offer. Extracted for
/// testability (codex: an expired OAuth credential must not suppress the
/// offer and then fail at runtime).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyCapturePreflight {
    EnvSet,
    ValidStoredCredential,
    Offer,
}

fn key_capture_preflight(
    env_set: bool,
    stored: Option<&crate::auth::AuthCredential>,
) -> KeyCapturePreflight {
    if env_set {
        return KeyCapturePreflight::EnvSet;
    }
    match stored {
        Some(cred) if !cred.is_expired() => KeyCapturePreflight::ValidStoredCredential,
        _ => KeyCapturePreflight::Offer,
    }
}

/// Init-time credential offer, mirroring the runtime resolution order:
/// env var set → done; global auth store already holds a NON-EXPIRED
/// credential → done; otherwise, on an interactive TTY run, offer to
/// paste the key now (Enter skips). Non-interactive runs (`--defaults`,
/// flag-driven custom, no TTY) keep the old export hint — they must
/// never block on a read (codex: `--defaults` is documented to skip
/// interactive prompts).
fn offer_api_key_capture(provider: &str, api_key_env: &str, interactive: bool) {
    use std::io::IsTerminal as _;

    let auth_home = crate::config_context::resolve_config_context(None).auth_home;
    let store = crate::auth::AuthStore::at(&auth_home);
    let stored = store.as_ref().ok().and_then(|s| s.get(provider));
    match key_capture_preflight(std::env::var(api_key_env).is_ok(), stored) {
        KeyCapturePreflight::EnvSet => {
            println!("{} {} is set", "✓".green(), api_key_env);
            return;
        }
        KeyCapturePreflight::ValidStoredCredential => {
            println!(
                "{} credential for {} already saved (octos auth login)",
                "✓".green(),
                provider
            );
            return;
        }
        KeyCapturePreflight::Offer => {}
    }

    println!("{} {} is not set", "Warning:".yellow(), api_key_env);

    let captured = match store {
        Ok(mut s) if interactive && std::io::stdin().is_terminal() && !provider.is_empty() => {
            println!("Paste your {provider} API key now to save it securely (Enter to skip):");
            print!("> ");
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let mut stdin = std::io::stdin().lock();
            capture_api_key_from_reader(provider, &mut s, &mut stdin)
                .unwrap_or(KeyCaptureOutcome::Skipped)
        }
        _ => KeyCaptureOutcome::Skipped,
    };

    match captured {
        KeyCaptureOutcome::Saved => {
            println!(
                "{} Key saved for {} — chat/serve will use it (auth store)",
                "✓".green(),
                provider
            );
            println!();
        }
        KeyCaptureOutcome::Skipped => {
            println!();
            println!("Set it later with:");
            println!("  octos auth login --provider {provider}");
            println!("  # or: export {api_key_env}=your-api-key");
            println!();
        }
    }
}

/// produce an identical, fully-bootstrapped octos home.
fn write_init_files(
    config_dir: PathBuf,
    config_path: PathBuf,
    config: Value,
    api_key_env: String,
    interactive: bool,
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
    println!("{config_str}");
    println!();

    // Credential check. Resolution order at runtime is auth store first,
    // then env var (`Config::get_api_key`), so mirror that here — and when
    // NEITHER holds a credential, offer to capture the key on the spot
    // through the same global auth store `octos auth login` writes (#1541:
    // init used to stop at "export it yourself", leaving a fresh setup
    // that cannot actually reach the provider).
    let provider_name = config
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    offer_api_key_capture(&provider_name, &api_key_env, interactive);

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

// The repo's model catalog, baked in at compile time so INSTALLED binaries
// (brew/npm/installer bundles ship no model_catalog.json on disk) still offer
// real model names in `octos init` — pre-fix, every provider fell into manual
// entry whose "auto" default is rejected by real APIs (#1541 item 3). The single
// embedded copy lives in `qos_catalog`; alias it here so there is exactly one
// compiled-in `model_catalog.json` across the crate.
use crate::qos_catalog::EMBEDDED_MODEL_CATALOG;

/// Parse a model_catalog.json payload into provider → model-name lists.
fn parse_catalog_content(content: &str) -> BTreeMap<String, Vec<String>> {
    let mut result: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Ok(catalog) = serde_json::from_str::<serde_json::Value>(content) {
        if let Some(models) = catalog.get("models").and_then(|m| m.as_array()) {
            for model in models {
                if let Some(provider_model) = model.get("provider").and_then(|p| p.as_str()) {
                    let parts: Vec<&str> = provider_model.splitn(2, '/').collect();
                    if parts.len() == 2 {
                        result
                            .entry(parts[0].to_string())
                            .or_default()
                            .push(parts[1].to_string());
                    }
                }
            }
        }
    }
    result
}

/// The embedded catalog, parsed (compile-time fallback for installs).
fn embedded_catalog_models() -> BTreeMap<String, Vec<String>> {
    parse_catalog_content(EMBEDDED_MODEL_CATALOG)
}

/// Read a model name, re-prompting until non-empty — there is NO silent
/// default: "auto" is not a real model on any current provider API and
/// writing it produced a config that 400s on the first turn.
fn read_required_model(
    reader: &mut dyn std::io::BufRead,
    provider_display: &str,
) -> Result<String> {
    for _ in 0..16 {
        print!("Model (e.g. the provider's current model name): ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    eyre::bail!(
        "a model name is required for {provider_display} — check the provider's docs for current model names"
    )
}

/// Resolve the non-interactive (`--defaults`) model for a provider:
/// hardcoded knowns, then the catalog's first entry. Anything else is an
/// error — `--defaults` must never silently write "auto".
fn default_model_for(provider: &str, catalog: &BTreeMap<String, Vec<String>>) -> Result<String> {
    match provider {
        "openai" => return Ok("gpt-4.1-mini".to_string()),
        "anthropic" => return Ok("claude-sonnet-4-20250514".to_string()),
        _ => {}
    }
    if let Some(first) = catalog.get(provider).and_then(|m| m.first()) {
        return Ok(first.clone());
    }
    eyre::bail!(
        "no known default model for provider '{provider}' — run `octos init` interactively and enter a model name"
    )
}

/// Load models from model_catalog.json, grouped by provider — from the
/// usual disk locations first (repo/dev flows), else the embedded
/// compile-time copy (installed binaries ship no catalog file).
fn load_catalog_models() -> BTreeMap<String, Vec<String>> {
    let candidates = [
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("model_catalog.json"))),
        dirs::home_dir().map(|d| d.join(".octos").join("model_catalog.json")),
        Some(PathBuf::from("model_catalog.json")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if let Ok(content) = std::fs::read_to_string(&candidate) {
            let parsed = parse_catalog_content(&content);
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }

    embedded_catalog_models()
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
            // Flag-driven: documented to run without prompts.
            return write_init_files(
                config_dir,
                config_path,
                custom.to_json(),
                api_key_env,
                false,
            );
        }

        // Load model catalog for hints
        let catalog = load_catalog_models();

        let (provider_info_idx, model, api_key_env, api_selection) = if self.defaults {
            // Auto-detect from env vars, or prompt if none found
            let idx = detect_from_env().unwrap_or(0); // fallback to first (openai)
            let info = &PROVIDERS[idx];
            let default_model = default_model_for(info.name, &catalog)?;
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
                            true,
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

            // Model selection — show from catalog if available (disk or the
            // embedded compile-time copy). Without catalog entries there is
            // NO default: "auto" is not a real model on any provider API
            // (#1541 item 3), so manual entry requires an explicit name and
            // is FINAL — no second confirm prompt after it (codex: a
            // fall-through re-prompt consumed the next piped stdin answer,
            // writing e.g. the env-var reply as the model).
            let model = match catalog.get(info.name).filter(|m| !m.is_empty()) {
                Some(models) => {
                    println!();
                    println!("Available models for {} (from catalog):", info.display);
                    for (i, m) in models.iter().enumerate() {
                        let rec = if i == 0 { " (recommended)" } else { "" };
                        println!("  - {m}{rec}");
                    }
                    let default_model = models[0].clone();
                    println!();
                    print!("Model [{default_model}]: ");
                    io::stdout().flush()?;

                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;
                    if input.trim().is_empty() {
                        default_model
                    } else {
                        input.trim().to_string()
                    }
                }
                None => {
                    println!();
                    println!(
                        "No catalog models found for {}. Enter model name manually:",
                        info.display
                    );
                    let mut stdin = io::stdin().lock();
                    read_required_model(&mut stdin, info.display)?
                }
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

        // `--defaults` is documented to skip interactive prompts — the
        // credential-capture offer must not block on a read there.
        write_init_files(config_dir, config_path, config, api_key_env, !self.defaults)
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

        // Never silently write model "auto" for a custom endpoint (#1541): real
        // OpenAI-compatible APIs reject "auto" with a 400. Require an explicit
        // --custom-model, matching the interactive and --defaults guards.
        let Some(model) = self.custom_model.clone() else {
            eyre::bail!(
                "--custom-model is required when using --custom-base-url (a custom endpoint has no \"auto\" model)"
            );
        };
        Ok(Some(SelectedProvider {
            provider: CUSTOM_PROVIDER_NAME.to_string(),
            model,
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

    // #1541: `octos init` offers to capture the API key right after the
    // provider choice, storing it through the SAME global auth store that
    // `octos auth login` writes and `Config::get_api_key` reads first —
    // instead of stopping at "Warning: X is not set, export it yourself".

    #[test]
    fn should_save_pasted_key_to_auth_store_when_input_nonempty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut store = crate::auth::AuthStore::at(tmp.path()).unwrap();
        let mut input = std::io::Cursor::new(b"sk-test-abc123\n".to_vec());

        let outcome = capture_api_key_from_reader("deepseek", &mut store, &mut input).unwrap();

        assert_eq!(outcome, KeyCaptureOutcome::Saved);
        let cred = store.get("deepseek").expect("credential saved");
        assert_eq!(cred.access_token, "sk-test-abc123");
        assert_eq!(cred.auth_method, "paste_token");
        // Durable: a fresh store handle at the same auth_home sees it (the
        // whole point — chat/serve resolve the GLOBAL store, not process env).
        let reopened = crate::auth::AuthStore::at(tmp.path()).unwrap();
        assert_eq!(
            reopened.get("deepseek").map(|c| c.access_token.as_str()),
            Some("sk-test-abc123")
        );
    }

    #[test]
    fn should_skip_without_writing_when_input_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut store = crate::auth::AuthStore::at(tmp.path()).unwrap();
        let mut input = std::io::Cursor::new(b"\n".to_vec());

        let outcome = capture_api_key_from_reader("deepseek", &mut store, &mut input).unwrap();

        assert_eq!(outcome, KeyCaptureOutcome::Skipped);
        assert!(store.get("deepseek").is_none());
        assert!(
            !tmp.path().join("auth.json").exists(),
            "an empty paste must not create/touch auth.json"
        );
    }

    // #1541 item 3: installed binaries ship no model_catalog.json on disk,
    // so every provider fell into manual entry whose default — "auto" — is
    // rejected by real APIs (DeepSeek 400s on it). The catalog is now
    // embedded as a compile-time fallback, manual entry requires an explicit
    // model name, and --defaults errors instead of silently writing "auto".

    // octos#2125: the MiniMax China preset is a first-class region variant —
    // the Token-plan flow (preset -> protocol -> model -> config) must write
    // the api.minimaxi.com endpoint with no manual base_url override.

    #[test]
    fn minimax_cn_preset_writes_the_china_endpoint() {
        let info = provider("minimax-cn");
        assert_eq!(info.api_key_env, "MINIMAX_CN_API_KEY");
        assert_eq!(
            default_api_type_selection(info).base_url,
            Some("https://api.minimaxi.com/v1")
        );
        // Both protocols the China platform documents are offered, OpenAI
        // first (the default).
        assert_eq!(info.api_types.len(), 2);
        let anthropic = select_api_type(info, "2");
        assert_eq!(anthropic.api_type, Some("anthropic"));
        assert_eq!(
            anthropic.base_url,
            Some("https://api.minimaxi.com/anthropic")
        );

        let config = build_config(
            info,
            "MiniMax-M3",
            info.api_key_env,
            default_api_type_selection(info),
        );
        assert_eq!(config["provider"], "minimax-cn");
        assert_eq!(config["base_url"], "https://api.minimaxi.com/v1");
        assert!(config.get("api_type").is_none());

        // The catalog offers the Token-plan model for the family.
        let catalog = embedded_catalog_models();
        assert_eq!(
            default_model_for("minimax-cn", &catalog).as_deref().ok(),
            Some("MiniMax-M3")
        );
    }

    #[test]
    fn existing_preset_indices_do_not_shift() {
        // Piped/scripted init flows select by number; only the trailing
        // Custom entry (PROVIDERS.len() + 1) may move when a preset is added.
        for (index, name) in [
            "openai",
            "anthropic",
            "gemini",
            "deepseek",
            "moonshot",
            "dashscope",
            "minimax",
            "zai",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(PROVIDERS[index].name, *name, "preset {index} shifted");
        }
        assert_eq!(PROVIDERS.last().unwrap().name, "minimax-cn");
    }

    #[test]
    fn should_resolve_models_from_embedded_catalog_when_no_disk_file() {
        let models = embedded_catalog_models();
        let deepseek = models
            .get("deepseek")
            .expect("deepseek in embedded catalog");
        assert!(
            deepseek.iter().any(|m| m.starts_with("deepseek-")),
            "embedded catalog offers a real deepseek model, got {deepseek:?}"
        );
        assert!(models.contains_key("openai"), "openai present too");
    }

    #[test]
    fn should_require_nonempty_model_when_no_catalog_default() {
        // skips blank lines until a real name is typed…
        let mut input = std::io::Cursor::new(b"\n\n  deepseek-v4-flash \n".to_vec());
        let model = read_required_model(&mut input, "DeepSeek").unwrap();
        assert_eq!(model, "deepseek-v4-flash");
        // …and EOF without a name is an error, never a silent "auto".
        let mut empty = std::io::Cursor::new(b"\n\n".to_vec());
        let err = read_required_model(&mut empty, "DeepSeek").unwrap_err();
        assert!(
            err.to_string().contains("model"),
            "err names the model: {err}"
        );
    }

    #[test]
    fn should_error_defaults_when_provider_has_no_known_model() {
        // --defaults must never write "auto": known hardcoded + catalog
        // providers resolve; anything else errors with guidance.
        let catalog = embedded_catalog_models();
        assert!(default_model_for("anthropic", &catalog).is_ok());
        assert!(default_model_for("deepseek", &catalog).is_ok());
        let err = default_model_for("no-such-provider", &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("model"), "guidance in {err}");
    }

    #[test]
    fn should_offer_capture_when_stored_credential_is_expired() {
        // codex fold: an expired OAuth credential must NOT suppress the
        // offer — Config::get_api_key rejects expired credentials, so
        // treating one as "configured" reports a working setup that
        // fails at runtime.
        let expired = crate::auth::AuthCredential {
            access_token: "stale".into(),
            refresh_token: None,
            expires_at: Some(chrono::DateTime::from_timestamp(1, 0).unwrap()), // long expired
            provider: "openai".into(),
            auth_method: "oauth".into(),
        };
        assert_eq!(
            key_capture_preflight(false, Some(&expired)),
            KeyCapturePreflight::Offer
        );

        let valid = crate::auth::AuthCredential {
            access_token: "fresh".into(),
            refresh_token: None,
            expires_at: None, // paste tokens never expire
            provider: "deepseek".into(),
            auth_method: "paste_token".into(),
        };
        assert_eq!(
            key_capture_preflight(false, Some(&valid)),
            KeyCapturePreflight::ValidStoredCredential
        );
        assert_eq!(
            key_capture_preflight(true, None),
            KeyCapturePreflight::EnvSet
        );
        assert_eq!(
            key_capture_preflight(false, None),
            KeyCapturePreflight::Offer
        );
    }

    #[test]
    fn should_trim_whitespace_when_key_pasted_with_newline_or_spaces() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut store = crate::auth::AuthStore::at(tmp.path()).unwrap();
        let mut input = std::io::Cursor::new(b"  sk-padded-key  \n".to_vec());

        let outcome = capture_api_key_from_reader("kimi", &mut store, &mut input).unwrap();

        assert_eq!(outcome, KeyCaptureOutcome::Saved);
        assert_eq!(store.get("kimi").unwrap().access_token, "sk-padded-key");
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
    fn custom_flags_require_model() {
        // Regression (#1541): --custom-base-url without --custom-model must error
        // rather than silently writing model "auto" (which real APIs 400 on).
        let cmd = InitCommand {
            cwd: None,
            defaults: false,
            force: false,
            custom_base_url: Some("https://api.example.com/v1".to_string()),
            custom_model: None,
            custom_api_type: None,
            custom_api_key_env: None,
        };

        let err = cmd.custom_selection_from_flags().unwrap_err();

        assert!(err.to_string().contains("--custom-model is required"));
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
