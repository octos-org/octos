use std::sync::Arc;

use eyre::Result;

use crate::anthropic::AnthropicProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// Z.AI uses the Anthropic Messages API protocol.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "zai",
    aliases: &["z.ai"],
    api_key_env: Some("ZAI_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some("https://api.z.ai/api/anthropic"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // Z.AI hosts multiple model families — no simple detect pattern.
    detect_patterns: &[],
    model_discovery: crate::discovery::ANTHROPIC_MODELS,
    model_discovery_for_model: None,
    create,
};

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
        .unwrap_or_else(|| "https://api.z.ai/api/anthropic".into());
    let mut provider = AnthropicProvider::new(&key, &model)
        .with_provider_label("zai")
        .with_base_url(&url)
        // Anthropic Messages-compatible by contract: `cache_control`
        // breakpoints are accepted, so keep caching ON instead of the
        // official-only default `with_base_url` applies to unknown hosts.
        .with_prompt_caching(true);
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

    #[tokio::test]
    async fn should_send_cache_breakpoints_when_zai_lane_is_built_from_registry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
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
            model: Some("glm-4.7".into()),
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
            "Anthropic-compatible zai lane must keep explicit cache breakpoints: {body}"
        );
    }
}
