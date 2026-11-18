use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// #2219 — OrcaRouter: an OpenAI-compatible aggregator (same category as
/// OpenRouter). Docs: <https://docs.orcarouter.ai>. Base URL and the
/// provider-prefixed model ids (`openai/gpt-4o-mini`,
/// `anthropic/claude-sonnet-4.6`, …) come from the official quickstart and
/// models pages; `/v1/models` is the standard OpenAI list endpoint, so
/// model discovery reuses OPENAI_MODELS. Only the chat-completions surface
/// is wired here — OrcaRouter-specific surfaces (image/video generation,
/// prompt caching, budgets, team access controls) are NOT claimed or
/// exposed; the generic OpenAI-compatible path remains for everything else.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "orcarouter",
    aliases: &[],
    api_key_env: Some("ORCAROUTER_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some("https://api.orcarouter.ai/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Models are provider-prefixed (`openai/…`, `anthropic/…`, …); no
    // un-prefixed detect pattern is safe to claim.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    model_discovery_for_model: None,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("ORCAROUTER_API_KEY not set"))?;
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
        .unwrap_or_else(|| "https://api.orcarouter.ai/v1".into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("orcarouter")
        .with_base_url(&url);
    // #2219 review — hints must be derived from the UPSTREAM model name
    // (`gpt-5`), not the full provider-prefixed id (`openai/gpt-5`):
    // ModelHints::detect matches prefixes at the string start, so the full
    // id would silently lose GPT-5's request settings (max_tokens instead
    // of max_completion_tokens, forwarded temperature, dropped reasoning
    // effort). The full id is passed to OpenAIProvider::new unchanged (the
    // wire keeps `openai/gpt-5`); the detect input is only the last path
    // segment. Explicit hint overrides are applied AFTER the detected
    // defaults, so they still win.
    let hints = crate::openai::ModelHints::detect(model.rsplit('/').next().unwrap_or(&model));
    provider = provider.with_hints(hints);
    if let Some(overrides) = p.model_hints {
        provider = provider.with_hints(overrides);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChatConfig, ReasoningEffort};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// #2219 review — a provider-prefixed id keeps its full form on the wire
    /// while hints come from the UPSTREAM name: `openai/gpt-5` must emit
    /// `max_completion_tokens` (never `max_tokens`), omit `temperature`,
    /// and keep the configured reasoning effort.
    #[tokio::test]
    async fn should_apply_gpt5_hints_to_a_prefixed_orcarouter_model_on_the_wire() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            ))
            .mount(&server)
            .await;
        let provider = create(CreateParams {
            api_key: Some("sk-orca-test".into()),
            model: Some("openai/gpt-5".into()),
            base_url: Some(server.uri()),
            model_hints: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        })
        .unwrap();
        let config = ChatConfig {
            max_tokens: Some(777),
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        provider
            .chat(&[octos_core::Message::user("hi")], &[], &config)
            .await
            .expect("mock chat");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "openai/gpt-5", "full id on the wire");
        assert_eq!(body["max_completion_tokens"], 777);
        assert!(body.get("max_tokens").is_none(), "{body}");
        assert!(body.get("temperature").is_none(), "{body}");
        assert_eq!(body["reasoning_effort"], "high");
    }

    /// #2219 review — explicit `model_hints` still WIN over the detected
    /// defaults: with a fully-default override the same prefixed model sends
    /// plain `max_tokens`, forwards the configured temperature, and drops
    /// the reasoning effort (ReasoningStyle::None).
    #[tokio::test]
    async fn should_let_explicit_hint_overrides_win_over_detected_gpt5_defaults() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            ))
            .mount(&server)
            .await;
        let provider = create(CreateParams {
            api_key: Some("sk-orca-test".into()),
            model: Some("openai/gpt-5".into()),
            base_url: Some(server.uri()),
            model_hints: Some(crate::openai::ModelHints::default()),
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        })
        .unwrap();
        let config = ChatConfig {
            max_tokens: Some(777),
            temperature: Some(0.7),
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        provider
            .chat(&[octos_core::Message::user("hi")], &[], &config)
            .await
            .expect("mock chat");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "openai/gpt-5", "full id on the wire");
        assert_eq!(
            body["max_tokens"], 777,
            "explicit override keeps max_tokens"
        );
        assert!(body.get("max_completion_tokens").is_none(), "{body}");
        assert_eq!(body["temperature"], 0.7);
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }
}
