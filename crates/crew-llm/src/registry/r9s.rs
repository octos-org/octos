use std::sync::Arc;

use eyre::Result;

use crate::anthropic::AnthropicProvider;
use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "r9s",
    aliases: &["r9s.ai"],
    default_model: Some("claude-sonnet-4-6"),
    api_key_env: Some("R9S_API_KEY"),
    default_base_url: Some("https://api.r9s.ai/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // R9S hosts many providers — no simple detect pattern.
    detect_patterns: &[],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("R9S_API_KEY not set"))?;
    let model = p.model.unwrap_or_else(|| "claude-sonnet-4-6".into());
    let url = p.base_url.unwrap_or_else(|| "https://api.r9s.ai/v1".into());

    // Auto-detect protocol: Anthropic Messages API for claude-* models,
    // OpenAI Chat Completions for everything else.
    if model.starts_with("claude-") {
        let anthropic_url = url
            .strip_suffix("/v1")
            .map(|base| format!("{base}/anthropic"))
            .unwrap_or_else(|| format!("{url}/anthropic"));
        let mut provider = AnthropicProvider::new(&key, &model).with_base_url(&anthropic_url);
        if let Some((t, c)) = http_timeout {
            provider = provider.with_http_timeout(t, c);
        }
        Ok(Arc::new(provider))
    } else {
        let mut provider = OpenAIProvider::new(&key, &model).with_base_url(&url);
        if let Some((t, c)) = http_timeout {
            provider = provider.with_http_timeout(t, c);
        }
        Ok(Arc::new(provider))
    }
}
