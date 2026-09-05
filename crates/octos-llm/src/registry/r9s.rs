use std::sync::Arc;

use eyre::Result;

use crate::anthropic::AnthropicProvider;
use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// The default OpenAI-compatible API root. ONE const behind the ENTRY
/// declaration and both fallbacks (`create` + discovery) so serving and
/// probing can never drift onto different roots.
const DEFAULT_BASE_URL: &str = "https://api.r9s.ai/v1";

/// Whether r9s serves `model` over the Anthropic Messages API.
///
/// r9s auto-selects the Anthropic protocol for `claude-*` models and the
/// OpenAI Chat Completions protocol for everything else. This is the SINGLE
/// source of truth for that split so provider construction (below) and the
/// cache-pricing classifier (`crate::pricing`) can never diverge — a
/// divergence would price an OpenAI-protocol model at Anthropic cache rates
/// (or vice versa). Case-sensitive by construction: a mixed-case
/// "Claude-..." is NOT `claude-*` and is served over OpenAI.
pub(crate) fn prefers_anthropic(model: &str) -> bool {
    model.starts_with("claude-")
}

/// The Anthropic-protocol API root: claude-* is served at `{base}/anthropic`
/// (the `/v1` suffix belongs to the OpenAI-compatible root). ONE rewrite
/// shared by provider construction and model discovery so the two can never
/// probe/serve different roots.
fn anthropic_root(base_url: &str) -> String {
    base_url
        .strip_suffix("/v1")
        .map(|base| format!("{base}/anthropic"))
        .unwrap_or_else(|| format!("{base_url}/anthropic"))
}

/// Per-model discovery resolution (octos#2185): a claude-* selection must be
/// probed with the Anthropic Messages strategy against [`anthropic_root`] —
/// the family-wide OpenAI declaration would probe `{base}/models` with a
/// Bearer header the Anthropic endpoint does not speak. Anything else keeps
/// the family-wide OpenAI-compatible listing with no root rewrite.
fn discovery_for_model(
    model: &str,
    base_url: Option<&str>,
) -> (crate::discovery::ModelDiscovery, Option<String>) {
    if prefers_anthropic(model) {
        // Default root from the ENTRY declaration; the const fallback
        // mirrors `create` and is unreachable while the entry declares one.
        let base = base_url
            .or(ENTRY.default_base_url)
            .unwrap_or(DEFAULT_BASE_URL);
        (
            crate::discovery::ANTHROPIC_MODELS,
            Some(anthropic_root(base)),
        )
    } else {
        (crate::discovery::OPENAI_MODELS, None)
    }
}

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "r9s",
    aliases: &["r9s.ai"],
    api_key_env: Some("R9S_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some(DEFAULT_BASE_URL),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // R9S hosts many providers — no simple detect pattern.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    model_discovery_for_model: Some(discovery_for_model),
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("R9S_API_KEY not set"))?;
    let model = p
        .model
        .or_else(|| ENTRY.default_model().map(str::to_string))
        .ok_or_else(|| {
            eyre::eyre!(
                "{}: no model given and the catalog declares no default for this family",
                ENTRY.name
            )
        })?;
    let url = p.base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into());

    // Auto-detect protocol: Anthropic Messages API for claude-* models,
    // OpenAI Chat Completions for everything else.
    if prefers_anthropic(&model) {
        let mut provider = AnthropicProvider::new(&key, &model)
            .with_provider_label("r9s")
            .with_base_url(anthropic_root(&url));
        if let Some((t, c)) = http_timeout {
            provider = provider.with_http_timeout(t, c);
        }
        Ok(Arc::new(provider))
    } else {
        let mut provider = OpenAIProvider::new(&key, &model)
            .with_provider_label("r9s")
            .with_base_url(&url);
        if let Some(hints) = p.model_hints {
            provider = provider.with_hints(hints);
        }
        if let Some((t, c)) = http_timeout {
            provider = provider.with_http_timeout(t, c);
        }
        Ok(Arc::new(provider))
    }
}
