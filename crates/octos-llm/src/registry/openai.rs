use std::sync::Arc;

use eyre::Result;

use crate::local_context_probe::LocalContextProbe;
use crate::openai::OpenAIProvider;
use crate::openai_responses::{OpenAIResponsesProvider, is_responses_capable};
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "openai",
    aliases: &[],
    api_key_env: Some("OPENAI_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some("https://api.openai.com/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &["gpt"],
    model_discovery: crate::discovery::OPENAI_MODELS,
    model_discovery_for_model: None,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("OPENAI_API_KEY not set"))?;
    let model = p
        .model
        .or_else(|| ENTRY.default_model().map(str::to_string))
        .ok_or_else(|| {
            eyre::eyre!(
                "{}: no model given and the catalog declares no default for this family",
                ENTRY.name
            )
        })?;

    // Auto-detect: use Responses API for capable models when talking to OpenAI directly
    // (no custom base_url set, which would indicate a compatible provider).
    let is_openai_direct = p.base_url.is_none();
    if is_openai_direct && is_responses_capable(&model) {
        let mut provider = OpenAIResponsesProvider::new(&key, &model);
        if let Some((t, c)) = http_timeout {
            provider = provider.with_http_timeout(t, c);
        }
        return Ok(Arc::new(provider));
    }

    // OpenAI-compatible servers are also configured under the `openai`
    // family. Their loaded window can differ from both the model catalog
    // and its broad family aliases (e.g. qwen3.8-27b matching qwen3).
    let probe_url = p
        .base_url
        .as_deref()
        .filter(|url| Some(url.trim_end_matches('/')) != ENTRY.default_base_url)
        .map(str::to_owned);
    let mut provider = OpenAIProvider::new(&key, &model);
    if let Some(url) = p.base_url {
        provider = provider.with_base_url(&url);
    }
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    let provider: Arc<dyn LlmProvider> = Arc::new(provider);
    if let Some(url) = probe_url {
        return Ok(LocalContextProbe::new(
            provider,
            &url,
            Some(key),
            http_timeout,
        ));
    }
    Ok(provider)
}
