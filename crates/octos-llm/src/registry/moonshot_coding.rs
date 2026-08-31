use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// Kimi **Coding Plan** family. Distinct from the regular `moonshot` family: it
/// targets the subscription coding endpoint `https://api.kimi.com/coding/v1`
/// (OpenAI-compatible) rather than `api.moonshot.ai`, and its keys are the
/// `sk-kimi-…` coding-plan keys, which the standard Moonshot endpoints reject.
/// The plan exposes `k3` (Kimi K3 — up to 1M context on higher tiers, low/high/
/// max thinking) plus `kimi-for-coding` / `kimi-for-coding-highspeed`.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "moonshot-coding",
    aliases: &["kimi-coding"],
    api_key_env: Some("KIMI_CODING_API_KEY"),
    key_env_aliases: &["KIMI_API_KEY", "MOONSHOT_API_KEY"],
    default_base_url: Some("https://api.kimi.com/coding/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Selected explicitly by family, not auto-detected: the coding models (`k3`,
    // `kimi-for-coding`) must not be silently routed to the regular `moonshot`
    // endpoint, which would reject a coding-plan key.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("KIMI_CODING_API_KEY (Kimi coding plan) not set"))?;
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
        .unwrap_or_else(|| "https://api.kimi.com/coding/v1".into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("moonshot-coding")
        .with_base_url(&url);
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
