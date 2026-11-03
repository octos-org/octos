use std::sync::Arc;

use eyre::Result;

use crate::openai::{ModelHints, OpenAIProvider};
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
    // A saved route from before the protocol switch still names the
    // Anthropic-compatible root; map it to the coding-plan OpenAI root (logged)
    // instead of sending OpenAI-shaped requests to a URL that 404s them.
    let url = p
        .base_url
        .map(|url| super::zai::migrate_legacy_anthropic_root(&url, DEFAULT_BASE_URL, ENTRY.name))
        .unwrap_or_else(|| DEFAULT_BASE_URL.into());
    // Plain OpenAI chat shape: no `cache_control` (Z.AI's OpenAI root has
    // rejected the field outright — "Extra inputs are not permitted") and no
    // `prompt_cache_key` affinity, which Z.AI does not implement; caching is
    // server-side and automatic, and the provider already normalises
    // `prompt_tokens_details.cached_tokens` into `TokenUsage::cache_read_tokens`.
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("zai-coding")
        .with_base_url(&url);
    if p.model_hints.is_none() {
        // GLM Coding Plan rejects non-text content parts with code 1210.
        // Preserve the tool result's metadata and attachment path instead of
        // turning a read_file image into a provider-fatal image_url request.
        // An explicit route override still wins for compatible custom models.
        let mut hints = ModelHints::detect(&model);
        hints.lacks_vision = true;
        provider = provider.with_hints(hints);
    }
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

    fn image_tool_messages(dir: &std::path::Path) -> Vec<Message> {
        let image = dir.join("meeting.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        let mk = |role: octos_core::MessageRole, content: String| Message {
            role,
            content,
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
            source: None,
            participant: None,
        };
        let mut assistant = mk(octos_core::MessageRole::Assistant, String::new());
        assistant.tool_calls = Some(vec![octos_core::ToolCall {
            id: "call_image".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "meeting.png"}),
            metadata: None,
        }]);
        let mut tool = mk(
            octos_core::MessageRole::Tool,
            serde_json::json!({
                "path": "meeting.png",
                "format": "png",
                "mime_type": "image/png",
                "byte_length": 8,
                "shown_to_model": true,
            })
            .to_string(),
        );
        tool.tool_call_id = Some("call_image".into());
        tool.media = vec![image.to_string_lossy().into_owned()];
        vec![
            mk(
                octos_core::MessageRole::User,
                "Attach the existing image to your reply".into(),
            ),
            assistant,
            tool,
        ]
    }

    #[tokio::test]
    async fn should_replace_tool_images_with_delivery_metadata_for_text_only_glm() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(
                        r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"The image is attached."},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
                    )
                    .append_header("Content-Type", "application/json"),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let provider = create(params(server.uri())).unwrap();
        provider
            .chat(
                &image_tool_messages(dir.path()),
                &[],
                &ChatConfig::default(),
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "text-only GLM needs no failed retry");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(
            !body.to_string().contains("image_url"),
            "GLM Coding Plan accepts text content parts only: {body}"
        );
        let content = body["messages"][2]["content"].as_str().unwrap();
        assert!(content.contains("meeting.png"), "{content}");
        assert!(content.contains("image/png"), "{content}");
        assert!(content.contains("byte_length"), "{content}");
        assert!(content.contains("attachment"), "{content}");
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
    fn should_migrate_a_saved_anthropic_root_to_the_coding_plan_root() {
        assert_eq!(
            super::super::zai::migrate_legacy_anthropic_root(
                "https://api.z.ai/api/anthropic",
                DEFAULT_BASE_URL,
                ENTRY.name
            ),
            DEFAULT_BASE_URL
        );
    }

    #[test]
    fn should_default_to_the_coding_plan_openai_root() {
        assert_eq!(ENTRY.default_base_url, Some(DEFAULT_BASE_URL));
        assert!(DEFAULT_BASE_URL.ends_with("/api/coding/paas/v4"));
        assert_eq!(ENTRY.model_discovery, crate::discovery::OPENAI_MODELS);
    }
}
