use std::sync::Arc;

use octos_llm::{ContextWindowOverride, LlmProvider, RetryProvider, registry};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider(base_url: String) -> Arc<dyn LlmProvider> {
    (registry::lookup("openai").unwrap().create)(registry::CreateParams {
        api_key: Some("context-probe-test-key".into()),
        model: Some("qwen3.8-27b".into()),
        base_url: Some(base_url),
        model_hints: None,
        llm_timeout_secs: Some(2),
        llm_connect_timeout_secs: Some(1),
    })
    .unwrap()
}

#[tokio::test]
async fn custom_openai_endpoint_resolves_runtime_window_before_first_prompt() {
    // Both directions matter: a smaller server allocation must also beat the
    // catalog. A model-family constant cannot fix this endpoint-specific bug.
    for window in [32_768, 262_144] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer context-probe-test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "qwen3.8-27b", "max_model_len": window}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = RetryProvider::new(provider(format!("{}/v1", server.uri())));
        // Compaction reads this synchronous accessor before the first chat.
        provider.ensure_ready().await;
        assert_eq!(provider.context_window(), window);
        assert_eq!(provider.model_id(), "qwen3.8-27b");
        provider.ensure_ready().await;
        assert_eq!(provider.context_window(), window);
    }
}

#[tokio::test]
async fn custom_openai_endpoint_without_runtime_metadata_keeps_catalog_budget() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "qwen3.8-27b"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = provider(format!("{}/v1", server.uri()));
    let original_window = provider.context_window();
    provider.ensure_ready().await;
    assert_eq!(provider.context_window(), original_window);
}

#[tokio::test]
async fn explicit_context_override_still_wins_over_custom_endpoint_probe() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "qwen3.8-27b", "max_model_len": 262_144}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = ContextWindowOverride::new(provider(format!("{}/v1", server.uri())), 65_536);
    provider.ensure_ready().await;
    assert_eq!(provider.context_window(), 65_536);
}
