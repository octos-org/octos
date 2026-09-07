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
            .with_base_url(anthropic_root(&url))
            // Anthropic Messages-compatible by contract: `cache_control`
            // breakpoints are accepted, so keep caching ON instead of the
            // official-only default `with_base_url` applies to unknown hosts.
            .with_prompt_caching(true);
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

#[cfg(test)]
mod tests {
    use octos_core::Message;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::ChatConfig;

    #[tokio::test]
    async fn should_send_cache_breakpoints_when_r9s_claude_lane_is_built_from_registry() {
        let server = MockServer::start().await;
        // A base URL without the `/v1` suffix maps to `<base>/anthropic`.
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(
                        r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
                    )
                    .append_header("Content-Type", "application/json"),
            )
            .mount(&server)
            .await;

        let provider = create(CreateParams {
            api_key: Some("test-key".into()),
            model: Some("claude-sonnet-4-6".into()),
            base_url: Some(server.uri()),
            model_hints: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        })
        .unwrap();
        provider
            .chat(
                &[Message::system("sys"), Message::user("hi")],
                &[],
                &ChatConfig::default(),
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(
            body.to_string().contains("cache_control"),
            "Anthropic-compatible r9s claude lane must keep explicit cache breakpoints: {body}"
        );
    }
}
