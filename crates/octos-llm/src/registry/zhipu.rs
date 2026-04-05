use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "zhipu",
    aliases: &["glm"],
    default_model: Some("glm-4-plus"),
    api_key_env: Some("ZHIPU_API_KEY"),
    default_base_url: Some("https://open.bigmodel.cn/api/paas/v4"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &["glm"],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("ZHIPU_API_KEY not set"))?;
    let model = p.model.unwrap_or_else(|| "glm-4-plus".into());
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
