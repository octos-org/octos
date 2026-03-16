use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::openai_responses::{OpenAIResponsesProvider, is_responses_capable};
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "openai",
    aliases: &[],
    default_model: Some("gpt-4o"),
    api_key_env: Some("OPENAI_API_KEY"),
    default_base_url: Some("https://api.openai.com/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &["gpt"],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("OPENAI_API_KEY not set"))?;
    let model = p.model.unwrap_or_else(|| "gpt-4o".into());

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
    Ok(Arc::new(provider))
}
