use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "moonshot",
    aliases: &["kimi"],
    default_model: Some("kimi-k2.5"),
    api_key_env: Some("MOONSHOT_API_KEY"),
    default_base_url: Some("https://api.moonshot.ai/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &["kimi", "moonshot"],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("MOONSHOT_API_KEY not set"))?;
    let model = p.model.unwrap_or_else(|| "kimi-k2.5".into());
    let url = p
        .base_url
        .unwrap_or_else(|| "https://api.moonshot.ai/v1".into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_base_url(&url)
        .with_provider_label("moonshot");
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
