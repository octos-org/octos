use std::sync::Arc;

use eyre::Result;

use crate::gemini::GeminiProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "gemini",
    aliases: &["google"],
    api_key_env: Some("GEMINI_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some("https://generativelanguage.googleapis.com/v1beta"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &["gemini"],
    model_discovery: crate::discovery::GEMINI_MODELS,
    model_discovery_for_model: None,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("GEMINI_API_KEY not set"))?;
    let model = p
        .model
        .or_else(|| ENTRY.default_model().map(str::to_string))
        .ok_or_else(|| {
            eyre::eyre!(
                "{}: no model given and the catalog declares no default for this family",
                ENTRY.name
            )
        })?;
    let mut provider = GeminiProvider::new(&key, &model);
    if let Some(url) = p.base_url {
        provider = provider.with_base_url(&url);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
