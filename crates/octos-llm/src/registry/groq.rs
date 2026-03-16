use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "groq",
    aliases: &[],
    default_model: Some("llama-3.3-70b-versatile"),
    api_key_env: Some("GROQ_API_KEY"),
    default_base_url: Some("https://api.groq.com/openai/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Groq hosts many open-source models — llama and mixtral are common.
    detect_patterns: &["llama", "mixtral"],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("GROQ_API_KEY not set"))?;
    let model = p.model.unwrap_or_else(|| "llama-3.3-70b-versatile".into());
    let url = p
        .base_url
        .unwrap_or_else(|| "https://api.groq.com/openai/v1".into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_base_url(&url)
        .with_provider_label("groq");
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
