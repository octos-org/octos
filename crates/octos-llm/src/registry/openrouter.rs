use std::sync::Arc;

use eyre::Result;

use crate::openrouter::OpenRouterProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "openrouter",
    aliases: &[],
    default_model: Some("anthropic/claude-sonnet-4-20250514"),
    api_key_env: Some("OPENROUTER_API_KEY"),
    default_base_url: Some("https://openrouter.ai/api/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // OpenRouter hosts many models — no simple detect pattern.
    detect_patterns: &[],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("OPENROUTER_API_KEY not set"))?;
    let model = p
        .model
        .unwrap_or_else(|| "anthropic/claude-sonnet-4-20250514".into());
    let mut provider = OpenRouterProvider::new(&key, &model);
    if let Some(url) = p.base_url {
        provider = provider.with_base_url(&url);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
