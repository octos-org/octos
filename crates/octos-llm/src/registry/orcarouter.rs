use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// #2219 — OrcaRouter: an OpenAI-compatible aggregator (same category as
/// OpenRouter). Docs: <https://docs.orcarouter.ai>. Base URL and the
/// provider-prefixed model ids (`openai/gpt-4o-mini`,
/// `anthropic/claude-sonnet-4.6`, …) come from the official quickstart and
/// models pages; `/v1/models` is the standard OpenAI list endpoint, so
/// model discovery reuses OPENAI_MODELS. Only the chat-completions surface
/// is wired here — OrcaRouter-specific surfaces (image/video generation,
/// prompt caching, budgets, team access controls) are NOT claimed or
/// exposed; the generic OpenAI-compatible path remains for everything else.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "orcarouter",
    aliases: &[],
    api_key_env: Some("ORCAROUTER_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some("https://api.orcarouter.ai/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Models are provider-prefixed (`openai/…`, `anthropic/…`, …); no
    // un-prefixed detect pattern is safe to claim.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    model_discovery_for_model: None,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("ORCAROUTER_API_KEY not set"))?;
    let model = p
        .model
        .or_else(|| ENTRY.default_model().map(str::to_string))
        .ok_or_else(|| {
            eyre::eyre!(
                "{}: no model given and the catalog declares no default for this family",
                ENTRY.name
            )
        })?;
    let url = p
        .base_url
        .unwrap_or_else(|| "https://api.orcarouter.ai/v1".into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("orcarouter")
        .with_base_url(&url);
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
