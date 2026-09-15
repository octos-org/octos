use std::sync::Arc;

use eyre::Result;

use crate::anthropic::AnthropicProvider;
use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "r9s",
    aliases: &["r9s.ai"],
    api_key_env: Some("R9S_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some("https://api.r9s.ai/v1"),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // R9S hosts many providers — no simple detect pattern.
    detect_patterns: &[],
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
    let url = p.base_url.unwrap_or_else(|| "https://api.r9s.ai/v1".into());

    // Auto-detect protocol: Anthropic Messages API for claude-* models,
    // OpenAI Chat Completions for everything else.
    if model.starts_with("claude-") {
        let anthropic_url = url
            .strip_suffix("/v1")
            .map(|base| format!("{base}/anthropic"))
            .unwrap_or_else(|| format!("{url}/anthropic"));
        let mut provider = AnthropicProvider::new(&key, &model)
            .with_provider_label("r9s")
            .with_base_url(&anthropic_url)
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
