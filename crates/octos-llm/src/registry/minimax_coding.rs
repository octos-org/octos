use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// MiniMax's international M Plan (formerly Token/Coding Plan). It shares
/// the regular API root but requires a Subscription Key for plan-only models.
/// Keep its credential separate so a PAYG key cannot silently replace it.
/// https://platform.minimax.io/docs/guides/text-generation
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "minimax-coding",
    aliases: &["minimax-m-plan", "minimax-token-plan"],
    api_key_env: Some("MINIMAX_CODING_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some("https://api.minimax.io/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    model_discovery_for_model: None,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p.api_key.ok_or_else(|| {
        eyre::eyre!("MINIMAX_CODING_API_KEY (MiniMax M Plan Subscription Key) not set")
    })?;
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
        .unwrap_or_else(|| "https://api.minimax.io/v1".into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label(ENTRY.name)
        .with_base_url(&url);
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
