use std::sync::Arc;

use eyre::Result;

use crate::anthropic::AnthropicProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// Z.AI **GLM Coding Plan** family. Like the regular `zai` family it speaks the
/// Anthropic Messages protocol against `https://api.z.ai/api/anthropic` (the
/// Claude-Code integration endpoint), but it defaults to the GLM coding model
/// (`glm-5.2`) and takes the coding-plan key, so the plan is a first-class,
/// pre-wired option rather than a manual base-url + model override. Validated
/// end-to-end (`glm-5.2` completes turns through this family); a profile route
/// can still override `base_url` / `model` for a different GLM coding model.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "zai-coding",
    aliases: &["z.ai-coding", "glm-coding"],
    api_key_env: Some("ZAI_CODING_API_KEY"),
    key_env_aliases: &["ZAI_API_KEY"],
    default_base_url: Some("https://api.z.ai/api/anthropic"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Selected explicitly by family — not auto-detected from a bare `glm-*`
    // model, which the regular `zai`/`zhipu` families already handle.
    detect_patterns: &[],
    model_discovery: crate::discovery::ANTHROPIC_MODELS,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("ZAI_CODING_API_KEY (Z.AI GLM coding plan) not set"))?;
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
        .unwrap_or_else(|| "https://api.z.ai/api/anthropic".into());
    let mut provider = AnthropicProvider::new(&key, &model)
        .with_provider_label("zai-coding")
        .with_base_url(&url);
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
