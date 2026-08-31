use std::sync::Arc;

use eyre::Result;

use crate::anthropic::AnthropicProvider;
use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// Whether r9s serves `model` over the Anthropic Messages API.
///
/// r9s auto-selects the Anthropic protocol for `claude-*` models and the
/// OpenAI Chat Completions protocol for everything else. This is the SINGLE
/// source of truth for that split so provider construction (below) and the
/// cache-pricing classifier (`crate::pricing`) can never diverge — a
/// divergence would price an OpenAI-protocol model at Anthropic cache rates
/// (or vice versa). Case-sensitive by construction: a mixed-case
/// "Claude-..." is NOT `claude-*` and is served over OpenAI.
pub(crate) fn prefers_anthropic(model: &str) -> bool {
    model.starts_with("claude-")
}

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "r9s",
    aliases: &["r9s.ai"],
    api_key_env: Some("R9S_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some("https://api.r9s.ai/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // R9S hosts many providers — no simple detect pattern.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("R9S_API_KEY not set"))?;
    let model = p
        .model
        .or_else(|| ENTRY.default_model().map(str::to_string))
        .ok_or_else(|| {
            eyre::eyre!(
                "{}: no model given and the catalog declares no default for this family",
                ENTRY.name
            )
        })?;
    let url = p.base_url.unwrap_or_else(|| "https://api.r9s.ai/v1".into());

    // Auto-detect protocol: Anthropic Messages API for claude-* models,
    // OpenAI Chat Completions for everything else.
    if prefers_anthropic(&model) {
        let anthropic_url = url
            .strip_suffix("/v1")
            .map(|base| format!("{base}/anthropic"))
            .unwrap_or_else(|| format!("{url}/anthropic"));
        let mut provider = AnthropicProvider::new(&key, &model)
            .with_provider_label("r9s")
            .with_base_url(&anthropic_url);
        if let Some((t, c)) = http_timeout {
            provider = provider.with_http_timeout(t, c);
        }
        Ok(Arc::new(provider))
    } else {
        let mut provider = OpenAIProvider::new(&key, &model)
            .with_provider_label("r9s")
            .with_base_url(&url);
        if let Some(hints) = p.model_hints {
            provider = provider.with_hints(hints);
        }
        if let Some((t, c)) = http_timeout {
            provider = provider.with_http_timeout(t, c);
        }
        Ok(Arc::new(provider))
    }
}
