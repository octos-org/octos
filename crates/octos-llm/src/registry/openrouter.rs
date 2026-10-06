use std::sync::Arc;

use eyre::Result;

use crate::openrouter::OpenRouterProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "openrouter",
    aliases: &[],
    api_key_env: Some("OPENROUTER_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some("https://openrouter.ai/api/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // OpenRouter hosts many models — no simple detect pattern.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    model_discovery_for_model: None,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("OPENROUTER_API_KEY not set"))?;
    let model = p
        .model
        .or_else(|| ENTRY.default_model().map(str::to_string))
        .ok_or_else(|| {
            eyre::eyre!(
                "{}: no model given and the catalog declares no default for this family",
                ENTRY.name
            )
        })?;
    let mut provider = OpenRouterProvider::new(&key, &model);
    if let Some(url) = p.base_url {
        provider = provider.with_base_url(&url);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
