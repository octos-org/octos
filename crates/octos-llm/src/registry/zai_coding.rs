use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// Z.AI **GLM Coding Plan** family. Speaks the OpenAI Chat Completions
/// protocol against the coding-plan root `https://api.z.ai/api/coding/paas/v4`
/// and defaults to the GLM coding model, taking the coding-plan key, so the
/// plan is a first-class, pre-wired option rather than a manual base-url +
/// model override. A profile route can still override `base_url` / `model`.
///
/// Why OpenAI and not the Anthropic-compatible `/api/anthropic` root this lane
/// used to target: Z.AI's prompt cache is implicit only. It never honours
/// Anthropic `cache_control` breakpoints (the Anthropic-compatible root accepts
/// and ignores them, answering `cache_read_input_tokens: 0` on every request),
/// and it reports hits only on the OpenAI-compatible root, in
/// `usage.prompt_tokens_details.cached_tokens`. Measured on 21 Sep 2026 with
/// the same key and the same two-request agent turn: 0 cached tokens via
/// `/api/anthropic` versus ~12.7k cached of ~12.7k prompt tokens via
/// `/api/coding/paas/v4`, with the prefix surviving across sessions. An agent
/// loop on the old root re-billed its whole context on every iteration.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "zai-coding",
    aliases: &["z.ai-coding", "glm-coding"],
    api_key_env: Some("ZAI_CODING_API_KEY"),
    key_env_aliases: &["ZAI_API_KEY"],
    default_base_url: Some(DEFAULT_BASE_URL),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Selected explicitly by family — not auto-detected from a bare `glm-*`
    // model, which the regular `zai`/`zhipu` families already handle.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    model_discovery_for_model: None,
    create,
};

/// The GLM Coding Plan's OpenAI-compatible root (versioned: `.../v4`, so the
/// discovery probe must not append another version segment).
pub const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

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
    let url = p.base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into());
    // Plain OpenAI chat shape: no `cache_control` (Z.AI's OpenAI root has
    // rejected the field outright — "Extra inputs are not permitted") and no
    // `prompt_cache_key` affinity, which Z.AI does not implement; caching is
    // server-side and automatic, and the provider already normalises
    // `prompt_tokens_details.cached_tokens` into `TokenUsage::cache_read_tokens`.
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("zai-coding")
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
    use octos_core::Message;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::ChatConfig;

    fn params(base_url: String) -> CreateParams {
        CreateParams {
            api_key: Some("test-key".into()),
            model: Some("glm-5.3-flash".into()),
            base_url: Some(base_url),
            model_hints: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn should_speak_openai_chat_without_cache_control_when_zai_coding_lane_is_built() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(
                        r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":75}}}"#,
                    )
                    .append_header("Content-Type", "application/json"),
            )
            .mount(&server)
            .await;

        let provider = create(params(server.uri())).unwrap();
        let response = provider
            .chat(
                &[Message::system("sys"), Message::user("hi")],
                &[],
                &ChatConfig::default(),
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "one chat completion request");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "glm-5.3-flash");
        assert!(
            !body.to_string().contains("cache_control"),
            "Z.AI's OpenAI root rejects Anthropic cache_control; the lane must not send it: {body}"
        );
        assert!(
            body.get("prompt_cache_key").is_none(),
            "Z.AI does not implement prompt_cache_key affinity: {body}"
        );

        // Z.AI's implicit cache is reported in prompt_tokens_details and must
        // land in the normalised disjoint accounting (input excludes cached).
        let usage = response.usage;
        assert_eq!(usage.cache_read_tokens, 75);
        assert_eq!(usage.input_tokens, 25);
    }

    #[test]
    fn should_default_to_the_coding_plan_openai_root() {
        assert_eq!(ENTRY.default_base_url, Some(DEFAULT_BASE_URL));
        assert!(DEFAULT_BASE_URL.ends_with("/api/coding/paas/v4"));
        assert_eq!(ENTRY.model_discovery, crate::discovery::OPENAI_MODELS);
    }
}
