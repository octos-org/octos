//! Runtime LLM model switching tool for gateway users.
//!
//! Allows normal users chatting with the bot to list available providers
//! and switch to a different model at runtime. The old provider is kept
//! as a fallback via `ProviderChain`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use octos_agent::tools::{Tool, ToolResult};
use octos_llm::{LlmProvider, ProviderChain, RetryProvider, SwappableProvider};
use eyre::Result;
use serde::Deserialize;
use tracing::info;

use crate::config::Config;

/// Tool for listing available models and switching at runtime.
pub struct SwitchModelTool {
    swappable: Arc<SwappableProvider>,
    /// The original provider at gateway start — always used as the fallback
    /// so repeated swaps produce a flat chain `[new, original]` instead of
    /// nesting `Chain[new, Chain[prev, Chain[...]]]`.
    original_provider: Arc<dyn LlmProvider>,
    config: Config,
    profile_path: Option<PathBuf>,
}

impl SwitchModelTool {
    pub fn new(
        swappable: Arc<SwappableProvider>,
        config: Config,
        profile_path: Option<PathBuf>,
    ) -> Self {
        // Capture the current provider as the permanent fallback.
        let original_provider = swappable.current();
        Self {
            swappable,
            original_provider,
            config,
            profile_path,
        }
    }
}

#[derive(Deserialize)]
struct Input {
    action: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
}

#[async_trait]
impl Tool for SwitchModelTool {
    fn name(&self) -> &str {
        "switch_model"
    }

    fn description(&self) -> &str {
        "List available LLM providers or switch to a different model at runtime. \
         Use action='list' to see available providers and current config. \
         Use action='switch' with a model name to change the active model."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "switch"],
                    "description": "Action: 'list' to show available providers, 'switch' to change model"
                },
                "model": {
                    "type": "string",
                    "description": "Model name to switch to (e.g. 'deepseek-chat', 'gpt-4o', 'kimi-2.5'). Required for 'switch' action."
                },
                "provider": {
                    "type": "string",
                    "description": "Provider name. Auto-detected from model name if omitted."
                },
                "base_url": {
                    "type": "string",
                    "description": "Custom API base URL (for self-hosted providers)."
                },
                "api_key_env": {
                    "type": "string",
                    "description": "Environment variable name for API key (e.g. 'OPENAI_API_KEY')."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input = serde_json::from_value(args.clone())
            .map_err(|e| eyre::eyre!("invalid arguments: {e}"))?;

        match input.action.as_str() {
            "list" => self.handle_list(),
            "switch" => Ok(self.handle_switch(input).await),
            other => Ok(ToolResult {
                output: format!("Unknown action: '{other}'. Use 'list' or 'switch'."),
                success: false,
                ..Default::default()
            }),
        }
    }
}

impl SwitchModelTool {
    fn handle_list(&self) -> Result<ToolResult> {
        let (current_provider, current_model) = self.swappable.provider_info();

        let mut lines = Vec::new();
        lines.push(format!("Current model: {current_provider}/{current_model}"));
        lines.push(String::new());
        lines.push("Available providers:".to_string());

        for entry in octos_llm::registry::all_entries() {
            let key_status = if let Some(env_var) = entry.api_key_env {
                if std::env::var(env_var).is_ok() {
                    "ready"
                } else {
                    "needs API key"
                }
            } else {
                "no key needed"
            };

            let default = entry
                .default_model
                .map(|m| format!(" (default: {m})"))
                .unwrap_or_default();

            let aliases = if entry.aliases.is_empty() {
                String::new()
            } else {
                format!(" [aliases: {}]", entry.aliases.join(", "))
            };

            lines.push(format!(
                "  - {}{} [{}]{}",
                entry.name, default, key_status, aliases
            ));
        }

        // Show configured fallback models
        if !self.config.fallback_models.is_empty() {
            lines.push(String::new());
            lines.push("Configured fallback models:".to_string());
            for fb in &self.config.fallback_models {
                let model = fb.model.as_deref().unwrap_or("default");
                lines.push(format!("  - {}/{}", fb.provider, model));
            }
        }

        Ok(ToolResult {
            output: lines.join("\n"),
            success: true,
            ..Default::default()
        })
    }

    async fn handle_switch(&self, input: Input) -> ToolResult {
        let model_name = match input.model {
            Some(m) => m,
            None => {
                return ToolResult {
                    output: "Error: 'model' is required for switch action.".to_string(),
                    success: false,
                    ..Default::default()
                };
            }
        };

        // Detect provider from model name if not explicitly given
        let provider_name = match input
            .provider
            .or_else(|| octos_llm::registry::detect_provider(&model_name).map(String::from))
        {
            Some(name) => name,
            None => {
                return ToolResult {
                    output: format!(
                        "Cannot auto-detect provider for model '{model_name}'. \
                         Please specify the 'provider' parameter."
                    ),
                    success: false,
                    ..Default::default()
                };
            }
        };

        // Look up provider entry
        let entry = match octos_llm::registry::lookup(&provider_name) {
            Some(e) => e,
            None => {
                return ToolResult {
                    output: format!(
                        "Unknown provider: '{provider_name}'. \
                         Use action='list' to see available providers."
                    ),
                    success: false,
                    ..Default::default()
                };
            }
        };

        // Check API key availability
        let api_key_env = input.api_key_env.as_deref();
        let effective_env = api_key_env.or(entry.api_key_env);
        if entry.requires_api_key {
            if let Some(env_var) = effective_env {
                if std::env::var(env_var).is_err() {
                    return ToolResult {
                        output: format!(
                            "Error: API key not available. \
                             Set the {env_var} environment variable."
                        ),
                        success: false,
                        ..Default::default()
                    };
                }
            } else {
                return ToolResult {
                    output: format!(
                        "Error: Provider '{provider_name}' requires an API key \
                         but no env var is configured."
                    ),
                    success: false,
                    ..Default::default()
                };
            }
        }

        // Build config for provider creation
        let mut new_config = self.config.clone();
        if let Some(ref env_name) = input.api_key_env {
            new_config.api_key_env = Some(env_name.clone());
        }

        // Create the new provider
        let base_url = input.base_url.clone();
        let new_provider = match crate::commands::chat::create_provider_with_api_type(
            &provider_name,
            &new_config,
            Some(model_name.clone()),
            base_url.clone(),
            None,
        ) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    output: format!("Error creating provider: {e}"),
                    success: false,
                    ..Default::default()
                };
            }
        };

        let (old_provider_name, old_model) = self.swappable.provider_info();

        // Build a flat chain: new provider (with retry) → original provider as fallback.
        // Always uses the original provider (captured at construction) so repeated
        // swaps don't nest chains.
        let new_chain: Arc<dyn LlmProvider> = Arc::new(ProviderChain::new(vec![
            Arc::new(RetryProvider::new(new_provider)),
            self.original_provider.clone(),
        ]));

        // Atomic swap
        self.swappable.swap(new_chain);

        info!(
            old_provider = %old_provider_name,
            old_model = %old_model,
            new_provider = %provider_name,
            new_model = %model_name,
            "model switched via switch_model tool"
        );

        // Persist to profile JSON if available
        if let Some(ref profile_path) = self.profile_path {
            if let Err(e) = persist_to_profile(
                profile_path,
                &provider_name,
                &model_name,
                base_url.as_deref(),
                input.api_key_env.as_deref(),
            ) {
                info!(error = %e, "failed to persist model switch to profile");
            }
        }

        ToolResult {
            output: format!(
                "Switched to {provider_name}/{model_name}. \
                 Previous model ({old_provider_name}/{old_model}) is kept as fallback."
            ),
            success: true,
            ..Default::default()
        }
    }
}

/// Update the profile JSON file with the new provider/model config.
///
/// Uses atomic write-then-rename for crash safety.
fn persist_to_profile(
    profile_path: &std::path::Path,
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
) -> Result<()> {
    let content = std::fs::read_to_string(profile_path)?;
    let mut profile: serde_json::Value = serde_json::from_str(&content)?;

    if let Some(config) = profile.get_mut("config").and_then(|c| c.as_object_mut()) {
        config.insert("provider".into(), serde_json::json!(provider));
        config.insert("model".into(), serde_json::json!(model));
        if let Some(url) = base_url {
            config.insert("base_url".into(), serde_json::json!(url));
        } else {
            config.remove("base_url");
        }
        if let Some(env) = api_key_env {
            config.insert("api_key_env".into(), serde_json::json!(env));
        }
    }

    let updated = serde_json::to_string_pretty(&profile)?;

    // Atomic write: write to temp file then rename
    let dir = profile_path
        .parent()
        .ok_or_else(|| eyre::eyre!("profile path has no parent directory"))?;
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::fs::write(tmp.path(), &updated)?;
    tmp.persist(profile_path)?;

    info!(path = %profile_path.display(), "persisted model switch to profile");
    Ok(())
}
