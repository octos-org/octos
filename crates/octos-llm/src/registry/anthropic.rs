use std::sync::Arc;

use eyre::Result;

use crate::anthropic::AnthropicProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "anthropic",
    aliases: &[],
    default_model: Some("claude-sonnet-4-20250514"),
    api_key_env: Some("ANTHROPIC_API_KEY"),
    default_base_url: Some("https://api.anthropic.com"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &["claude"],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("ANTHROPIC_API_KEY not set"))?;
    let model = p.model.unwrap_or_else(|| "claude-sonnet-4-20250514".into());
    let mut provider = AnthropicProvider::new(&key, &model);
    if let Some(url) = p.base_url {
        provider = provider.with_base_url(&url);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
