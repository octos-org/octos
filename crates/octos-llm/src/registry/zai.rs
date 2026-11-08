use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// Z.AI's general (pay-as-you-go) API. Speaks the OpenAI Chat Completions
/// protocol against `https://api.z.ai/api/paas/v4`.
///
/// This lane used to target Z.AI's Anthropic-compatible root with explicit
/// `cache_control` breakpoints. Z.AI's prompt cache is implicit only: the
/// Anthropic-compatible root accepts the breakpoints and ignores them (every
/// request answers `cache_read_input_tokens: 0`), while the OpenAI-compatible
/// root caches the repeated prefix automatically and reports the hit in
/// `usage.prompt_tokens_details.cached_tokens`. See `zai_coding.rs` for the
/// measurement; the same applies to this root per Z.AI's context-caching docs.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "zai",
    aliases: &["z.ai"],
    api_key_env: Some("ZAI_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some(DEFAULT_BASE_URL),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Z.AI hosts multiple model families — no simple detect pattern.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    model_discovery_for_model: None,
    create,
};

/// Z.AI's OpenAI-compatible root (versioned: `.../v4`).
pub const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

/// Z.AI's Anthropic-compatible root, which these lanes targeted before they
/// moved to the OpenAI protocol. Saved routes still carry it as `base_url`.
pub(crate) const LEGACY_ANTHROPIC_ROOT: &str = "https://api.z.ai/api/anthropic";

/// Whether a saved `base_url` names Z.AI's Anthropic-compatible root.
pub(crate) fn is_legacy_anthropic_root(url: &str) -> bool {
    url.trim()
        .trim_end_matches('/')
        .eq_ignore_ascii_case(LEGACY_ANTHROPIC_ROOT)
}

/// Migrate a saved z.ai `base_url` that still names the Anthropic-compatible
/// root. The lane now speaks OpenAI Chat Completions, and sending that shape
/// to `/api/anthropic` fails with a 404, so the override is mapped to the
/// lane's OpenAI-compatible root and the migration is logged once per
/// provider build. Any other override is kept verbatim. A route that
/// explicitly sets `api_type: anthropic` never reaches this lane: the
/// api_type dispatch keeps it on the Anthropic protocol at its own URL (or,
/// without one, at this legacy root via
/// `registry::anthropic_api_type_default_root`), which remains the
/// (uncached) fallback for anyone who wants it.
pub(crate) fn migrate_legacy_anthropic_root(url: &str, replacement: &str, lane: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if is_legacy_anthropic_root(url) {
        tracing::warn!(
            lane,
            from = trimmed,
            to = replacement,
            "z.ai route base_url names the Anthropic-compatible root, which the {lane} lane no \
             longer speaks (OpenAI Chat Completions is the only Z.AI root that reports its \
             prompt cache); using the OpenAI-compatible root instead — update the saved route"
        );
        return replacement.to_owned();
    }
    url.to_owned()
}

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("ZAI_API_KEY not set"))?;
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
        .map(|url| migrate_legacy_anthropic_root(&url, DEFAULT_BASE_URL, ENTRY.name))
        .unwrap_or_else(|| DEFAULT_BASE_URL.into());
    // Plain OpenAI chat shape: no `cache_control` (Z.AI's OpenAI root rejects
    // the field) and no `prompt_cache_key` affinity (not implemented by
    // Z.AI); caching is server-side and automatic.
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("zai")
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
    fn should_migrate_a_saved_anthropic_root_and_keep_other_overrides() {
        // Saved routes from before the protocol switch.
        assert_eq!(
            migrate_legacy_anthropic_root(
                "https://api.z.ai/api/anthropic",
                DEFAULT_BASE_URL,
                "zai"
            ),
            DEFAULT_BASE_URL
        );
        assert_eq!(
            migrate_legacy_anthropic_root(
                "https://api.z.ai/api/anthropic/",
                DEFAULT_BASE_URL,
                "zai"
            ),
            DEFAULT_BASE_URL
        );
        // A genuine override (proxy, staging root) is untouched.
        assert_eq!(
            migrate_legacy_anthropic_root("http://127.0.0.1:9999/v4", DEFAULT_BASE_URL, "zai"),
            "http://127.0.0.1:9999/v4"
        );
    }
}
