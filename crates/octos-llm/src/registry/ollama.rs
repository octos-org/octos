use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "ollama",
    aliases: &[],
    default_model: Some("llama3.2"),
    api_key_env: None,
    default_base_url: Some("http://localhost:11434/v1"),
    requires_api_key: false,
    requires_base_url: false,
    requires_model: false,
    // Ollama hosts user-pulled models — no detect pattern.
    detect_patterns: &[],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let model = p.model.unwrap_or_else(|| "llama3.2".into());
    let url = p
        .base_url
        .unwrap_or_else(|| "http://localhost:11434/v1".into());
    let mut provider = OpenAIProvider::new("ollama", &model)
        .with_provider_label("ollama")
        .with_base_url(&url);
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
