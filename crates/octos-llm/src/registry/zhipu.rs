use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "zhipu",
    aliases: &["glm"],
    api_key_env: Some("ZHIPU_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some("https://open.bigmodel.cn/api/paas/v4"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &["glm"],
    model_discovery: crate::discovery::OPENAI_MODELS,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("ZHIPU_API_KEY not set"))?;
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
        .unwrap_or_else(|| "https://open.bigmodel.cn/api/paas/v4".into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("zhipu")
        .with_base_url(&url);
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
