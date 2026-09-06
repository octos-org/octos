use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// MiniMax **China** family. Distinct from the regular `minimax` family: it
/// targets the China endpoint `https://api.minimaxi.com/v1` (OpenAI-compatible)
/// rather than the international `https://api.minimax.io/v1`, because MiniMax
/// Token-plan subscription keys are issued by the China platform
/// (platform.minimaxi.com) and are region-bound — the international site
/// rejects them with a 401. Mirrors the `moonshot` / `moonshot-coding` split.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "minimax-cn",
    aliases: &["minimaxi"],
    api_key_env: Some("MINIMAX_CN_API_KEY"),
    key_env_aliases: &["MINIMAX_API_KEY"],
    default_base_url: Some("https://api.minimaxi.com/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Selected explicitly by family, not auto-detected: the models share the
    // base family's `MiniMax-*` names, so `detect_provider` keeps resolving
    // them to `minimax` (international) and the China endpoint is opt-in.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("MINIMAX_CN_API_KEY (MiniMax China) not set"))?;
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
        .unwrap_or_else(|| "https://api.minimaxi.com/v1".into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("minimax-cn")
        .with_base_url(&url);
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
