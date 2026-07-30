use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

const DEFAULT_MODEL: &str = "deepseek-ai/deepseek-v4-pro";
const DEFAULT_BASE_URL: &str = "https://api.atlascloud.ai/v1";

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "atlascloud",
    aliases: &["atlas-cloud"],
    default_model: Some(DEFAULT_MODEL),
    api_key_env: Some("ATLASCLOUD_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some(DEFAULT_BASE_URL),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &[],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("ATLASCLOUD_API_KEY not set"))?;
    let model = p.model.unwrap_or_else(|| DEFAULT_MODEL.into());
    let url = p.base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("atlascloud")
        .with_base_url(&url);
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_provider_with_default_route() {
        let provider = create(CreateParams {
            api_key: Some("test-key".into()),
            model: None,
            base_url: None,
            model_hints: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        })
        .expect("atlascloud provider should construct");

        assert_eq!(provider.model_id(), DEFAULT_MODEL);
        assert!(provider.provider_name().starts_with("atlascloud"));
    }
}
