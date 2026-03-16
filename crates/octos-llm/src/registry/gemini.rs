use std::sync::Arc;

use eyre::Result;

use crate::gemini::GeminiProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "gemini",
    aliases: &["google"],
    default_model: Some("gemini-2.5-flash"),
    api_key_env: Some("GEMINI_API_KEY"),
    default_base_url: Some("https://generativelanguage.googleapis.com/v1beta"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &["gemini"],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("GEMINI_API_KEY not set"))?;
    let model = p.model.unwrap_or_else(|| "gemini-2.5-flash".into());
    let mut provider = GeminiProvider::new(&key, &model);
    if let Some(url) = p.base_url {
        provider = provider.with_base_url(&url);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
