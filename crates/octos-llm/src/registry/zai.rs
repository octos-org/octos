use std::sync::Arc;

use eyre::Result;

use crate::anthropic::AnthropicProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// Z.AI uses the Anthropic Messages API protocol.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "zai",
    aliases: &["z.ai"],
    default_model: Some("glm-5"),
    api_key_env: Some("ZAI_API_KEY"),
    default_base_url: Some("https://api.z.ai/api/anthropic"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Z.AI hosts multiple model families — no simple detect pattern.
    detect_patterns: &[],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("ZAI_API_KEY not set"))?;
    let model = p.model.unwrap_or_else(|| "glm-5".into());
    let url = p
        .base_url
        .unwrap_or_else(|| "https://api.z.ai/api/anthropic".into());
    let mut provider = AnthropicProvider::new(&key, &model).with_base_url(&url);
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
