use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "nvidia",
    aliases: &["nim"],
    default_model: Some("meta/llama-3.3-70b-instruct"),
    api_key_env: Some("NVIDIA_API_KEY"),
    default_base_url: Some("https://integrate.api.nvidia.com/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Nvidia NIM hosts many models — no simple detect pattern.
    detect_patterns: &[],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("NVIDIA_API_KEY not set"))?;
    let model = p
        .model
        .unwrap_or_else(|| "meta/llama-3.3-70b-instruct".into());
    let url = p
        .base_url
        .unwrap_or_else(|| "https://integrate.api.nvidia.com/v1".into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("nvidia")
        .with_base_url(&url);
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
