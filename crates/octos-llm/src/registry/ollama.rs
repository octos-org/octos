use std::sync::Arc;

use eyre::Result;

use crate::local_context_probe::LocalContextProbe;
use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "ollama",
    aliases: &[],
    api_key_env: None,
    key_env_aliases: &[],
    default_base_url: Some("http://localhost:11434/v1"),
    requires_api_key: false,
    requires_base_url: false,
    requires_model: false,
    // Ollama hosts user-pulled models — no detect pattern.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    model_discovery_for_model: None,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
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
    // Same class of local server as the `local` family: the catalog's
    // context_window is a guess, the running server knows. Ollama serves
    // neither /props nor context metadata on /v1/models — its allocated
    // window lives on the native GET /api/ps, so the probe uses that
    // (#2135 review, P2).
    Ok(LocalContextProbe::new_ollama(
        Arc::new(provider),
        &url,
        http_timeout,
    ))
}
