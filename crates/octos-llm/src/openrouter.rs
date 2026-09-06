//! OpenRouter provider implementation (OpenAI-compatible API).

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::{Message, MessageRole};

use reqwest::Client;
use serde::{Deserialize, Serialize};

use secrecy::{ExposeSecret, SecretString};

use crate::cache_manifest::{PromptCacheInputManifest, without_cache_markers};
use crate::vision;

use crate::config::ChatConfig;
use crate::openai::parse_openai_sse_events;
use crate::provider::{LlmProvider, endpoint_label_from_base_url};
use crate::types::{ChatResponse, ChatStream, ProviderMetadata, StopReason, TokenUsage, ToolSpec};

/// OpenRouter provider (routes to many LLM providers).
pub struct OpenRouterProvider {
    client: Client,
    /// Separate client for streaming requests, built without a total request
    /// timeout so a healthy long generation is never cut off mid-stream. See
    /// [`crate::provider::build_streaming_http_client`].
    stream_client: Client,
    api_key: SecretString,
    model: String,
    base_url: String,
}

impl OpenRouterProvider {
    /// Create a new OpenRouter provider.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::provider::build_http_client(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
                crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            ),
            stream_client: crate::provider::build_streaming_http_client(
                crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            ),
            api_key: SecretString::from(api_key.into()),
            model: model.into(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
        }
    }

    /// Create a provider using the OPENROUTER_API_KEY environment variable.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .wrap_err("OPENROUTER_API_KEY environment variable not set")?;
        Ok(Self::new(api_key, "anthropic/claude-sonnet-4-20250514"))
    }

    /// Set a custom base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Replace the HTTP client with one using custom timeouts (in seconds).
    ///
    /// `timeout_secs` is the **total** request timeout for non-streaming
    /// requests. The streaming client is rebuilt only with the connect timeout —
    /// it never takes a total timeout, so a long streamed generation is not
    /// capped regardless of this value.
    pub fn with_http_timeout(mut self, timeout_secs: u64, connect_timeout_secs: u64) -> Self {
        self.client = crate::provider::build_http_client(timeout_secs, connect_timeout_secs);
        self.stream_client = crate::provider::build_streaming_http_client(connect_timeout_secs);
        self
    }

    /// Lane-attributed wording for operational failures (see
    /// [`crate::provider::operational_error_message`]).
    fn operational_message(&self, stage: crate::provider::OperationalStage) -> String {
        crate::provider::operational_error_message(
            stage,
            self.provider_name(),
            &self.model,
            crate::provider::ApiStyle::OpenRouterChatCompletions,
        )
    }

    fn prompt_cache_input_manifest_from_value(
        &self,
        request: &serde_json::Value,
        config: &ChatConfig,
    ) -> PromptCacheInputManifest {
        let normalized = without_cache_markers(request.clone());
        let mut stable = Vec::new();
        let mut conversation = Vec::new();
        if let Some(messages) = normalized
            .get("messages")
            .and_then(serde_json::Value::as_array)
        {
            for (index, message) in messages.iter().enumerate() {
                let role = message
                    .get("role")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let segment = (format!("message:{index}:{role}"), message.clone());
                if role == "system" || role == "developer" {
                    stable.push(segment);
                } else {
                    conversation.push(segment);
                }
            }
        }
        if let Some(tools) = normalized
            .get("tools")
            .and_then(serde_json::Value::as_array)
        {
            stable.extend(
                tools
                    .iter()
                    .enumerate()
                    .map(|(index, tool)| (format!("tool:{index}"), tool.clone())),
            );
        }
        PromptCacheInputManifest::from_normalized_segments(
            self.provider_name(),
            self.model.clone(),
            config
                .prompt_cache_context
                .as_ref()
                .map(|context| context.epoch_id.as_str()),
            stable,
            conversation,
        )
    }

    fn prompt_cache_input_manifest(
        &self,
        request: &ApiRequest<'_>,
        config: &ChatConfig,
    ) -> PromptCacheInputManifest {
        let request = serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({}));
        self.prompt_cache_input_manifest_from_value(&request, config)
    }

    fn trace_prompt_cache_input_value(&self, request: &serde_json::Value, config: &ChatConfig) {
        if tracing::enabled!(target: "octos.prompt_cache", tracing::Level::TRACE) {
            self.prompt_cache_input_manifest_from_value(request, config)
                .trace();
        }
    }

    fn trace_prompt_cache_input(&self, request: &ApiRequest<'_>, config: &ChatConfig) {
        if tracing::enabled!(target: "octos.prompt_cache", tracing::Level::TRACE) {
            self.prompt_cache_input_manifest(request, config).trace();
        }
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let api_messages: Vec<ApiMessage> = messages.iter().map(|m| build_api_message(m)).collect();

        let api_tools: Option<Vec<ApiTool>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| ApiTool {
                        r#type: "function",
                        function: ApiFunction {
                            name: &t.name,
                            description: &t.description,
                            parameters: &t.input_schema,
                        },
                    })
                    .collect(),
            )
        };

        let request = ApiRequest {
            model: &self.model,
            messages: api_messages,
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            tool_choice: config.tool_choice.openai_chat_wire(api_tools.is_some()),
            tools: api_tools,
        };
        self.trace_prompt_cache_input(&request, config);

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/heyong4725/octos")
            .header("X-Title", "octos")
            .json(&request)
            .send()
            .await
            .wrap_err_with(|| {
                crate::provider::transport_error_message(
                    false,
                    self.provider_name(),
                    &self.model,
                    crate::provider::ApiStyle::OpenRouterChatCompletions,
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let body = crate::provider::truncate_error_body(&body);
            // Route through LlmError so the harness classifier can pick the
            // user-facing variant rather than falling through to Internal/Bug.
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &body,
                format!("{}/{}", self.provider_name(), self.model),
            )
            .with_api_style(crate::provider::ApiStyle::OpenRouterChatCompletions)
            .into());
        }

        let api_response: ApiResponse = response.json().await.wrap_err_with(|| {
            self.operational_message(crate::provider::OperationalStage::ParseResponse)
        })?;

        openrouter_response_to_chat_response(api_response, &self.model)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let api_messages: Vec<ApiMessage> = messages.iter().map(|m| build_api_message(m)).collect();

        let api_tools: Option<Vec<ApiTool>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| ApiTool {
                        r#type: "function",
                        function: ApiFunction {
                            name: &t.name,
                            description: &t.description,
                            parameters: &t.input_schema,
                        },
                    })
                    .collect(),
            )
        };

        let request = ApiRequest {
            model: &self.model,
            messages: api_messages,
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            tool_choice: config.tool_choice.openai_chat_wire(api_tools.is_some()),
            tools: api_tools,
        };

        let mut body = serde_json::to_value(&request).wrap_err_with(|| {
            self.operational_message(crate::provider::OperationalStage::SerializeRequest)
        })?;
        let obj = body.as_object_mut().ok_or_else(|| {
            eyre::Report::msg(
                self.operational_message(crate::provider::OperationalStage::BuildRequestBody),
            )
        })?;
        obj.insert("stream".into(), true.into());
        obj.insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );
        self.trace_prompt_cache_input_value(&body, config);

        // Stream client: no total timeout, so a long healthy generation is not
        // cut off. Stalls are bounded by the client's per-read timeout and the
        // agent's stream-timeout guards (see build_streaming_http_client).
        let response = self
            .stream_client
            .post(format!("{}/chat/completions", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/heyong4725/octos")
            .header("X-Title", "octos")
            .json(&body)
            .send()
            .await
            .wrap_err_with(|| {
                crate::provider::transport_error_message(
                    true,
                    self.provider_name(),
                    &self.model,
                    crate::provider::ApiStyle::OpenRouterChatCompletions,
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let body = crate::provider::truncate_error_body(&text);
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &body,
                format!("{}/{}", self.provider_name(), self.model),
            )
            .with_api_style(crate::provider::ApiStyle::OpenRouterChatCompletions)
            .into());
        }

        let sse_stream = crate::sse::parse_sse_response(response);
        let event_stream =
            sse_stream.flat_map(|event| futures::stream::iter(parse_openai_sse_events(&event)));

        Ok(Box::pin(event_stream))
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "openrouter"
    }

    fn api_style(&self) -> Option<crate::provider::ApiStyle> {
        Some(crate::provider::ApiStyle::OpenRouterChatCompletions)
    }

    fn provider_metadata(&self) -> ProviderMetadata {
        let endpoint = if self.base_url != "https://openrouter.ai/api/v1" {
            endpoint_label_from_base_url(&self.base_url)
        } else {
            None
        };
        ProviderMetadata::new("openrouter", self.model.clone(), endpoint)
    }
}

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    messages: Vec<ApiMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ApiTool<'a>>>,
    /// `ChatConfig.tool_choice` on the wire (OpenAI chat form); absent for
    /// the default `auto`.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<ApiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ApiContent {
    Text(String),
    Parts(Vec<ApiContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ApiContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ApiImageUrl },
}

#[derive(Serialize)]
struct ApiImageUrl {
    url: String,
}

fn build_api_message<'a>(msg: &'a Message) -> ApiMessage<'a> {
    let role = msg.role.as_str();
    let content = build_api_content(msg);
    let tool_calls = msg.tool_calls.as_ref().map(|tcs| {
        tcs.iter()
            .map(|tc| ApiToolCall {
                id: tc.id.clone(),
                function: FunctionCall {
                    name: tc.name.clone(),
                    arguments: tc.arguments.to_string(),
                },
            })
            .collect()
    });
    ApiMessage {
        role,
        content,
        tool_call_id: msg.tool_call_id.as_deref(),
        tool_calls,
    }
}

fn build_api_content(msg: &Message) -> Option<ApiContent> {
    let images: Vec<_> = msg.media.iter().filter(|p| vision::is_image(p)).collect();

    if images.is_empty() {
        if msg.content.is_empty() {
            return match msg.role {
                MessageRole::User => Some(ApiContent::Text("[empty message]".to_string())),
                _ => None,
            };
        }
        return Some(ApiContent::Text(msg.content.clone()));
    }

    let mut parts = Vec::new();
    for path in images {
        if let Ok((mime, data)) = vision::encode_image(path) {
            parts.push(ApiContentPart::ImageUrl {
                image_url: ApiImageUrl {
                    url: format!("data:{mime};base64,{data}"),
                },
            });
        }
    }
    if !msg.content.is_empty() {
        parts.push(ApiContentPart::Text {
            text: msg.content.clone(),
        });
    }
    Some(ApiContent::Parts(parts))
}

#[derive(Serialize)]
struct ApiTool<'a> {
    r#type: &'a str,
    function: ApiFunction<'a>,
}

#[derive(Serialize)]
struct ApiFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Deserialize)]
struct ApiResponse {
    choices: Vec<Choice>,
    usage: Usage,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    finish_reason: String,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Serialize, Deserialize)]
struct ApiToolCall {
    id: String,
    function: FunctionCall,
}

#[derive(Serialize, Deserialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

fn openrouter_response_to_chat_response(
    api_response: ApiResponse,
    model: &str,
) -> Result<ChatResponse> {
    let ApiResponse { choices, usage } = api_response;
    let choice = choices.into_iter().next().ok_or_else(|| {
        eyre::Report::msg(crate::provider::operational_error_message(
            crate::provider::OperationalStage::NoChoices,
            "openrouter",
            model,
            crate::provider::ApiStyle::OpenRouterChatCompletions,
        ))
    })?;

    let ResponseMessage {
        content,
        reasoning,
        reasoning_content,
        tool_calls,
    } = choice.message;

    let tool_calls = tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|tc| octos_core::ToolCall {
            id: tc.id,
            name: tc.function.name,
            arguments: serde_json::from_str(&tc.function.arguments).unwrap_or_default(),
            metadata: None,
        })
        .collect();

    let stop_reason = match choice.finish_reason.as_str() {
        "stop" => StopReason::EndTurn,
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::ContentFiltered,
        _ => StopReason::EndTurn,
    };

    Ok(ChatResponse {
        content,
        reasoning_content: reasoning.or(reasoning_content),
        tool_calls,
        stop_reason,
        usage: TokenUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            ..Default::default()
        },
        provider_index: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PromptCacheContext;
    use octos_core::{Message, MessageRole};

    fn text_msg(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_build_api_content_text_only() {
        let msg = text_msg(MessageRole::User, "hello");
        let content = build_api_content(&msg);
        match content {
            Some(ApiContent::Text(t)) => assert_eq!(t, "hello"),
            other => panic!("expected Text, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_build_api_content_empty_user_gets_placeholder() {
        let msg = text_msg(MessageRole::User, "");
        let content = build_api_content(&msg);
        match content {
            Some(ApiContent::Text(t)) => assert_eq!(t, "[empty message]"),
            other => panic!("expected placeholder Text, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_build_api_content_empty_assistant_returns_none() {
        let msg = text_msg(MessageRole::Assistant, "");
        assert!(build_api_content(&msg).is_none());
    }

    #[test]
    fn test_build_api_content_empty_system_returns_none() {
        let msg = text_msg(MessageRole::System, "");
        assert!(build_api_content(&msg).is_none());
    }

    #[test]
    fn test_build_api_content_with_image() {
        let dir = tempfile::tempdir().unwrap();
        // Minimal valid PNG (1x1 pixel)
        let png_data: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xE2, 0x21, 0xBC,
            0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let img_path = dir.path().join("test.png");
        std::fs::write(&img_path, &png_data).unwrap();

        let msg = Message {
            role: MessageRole::User,
            content: "describe this".to_string(),
            media: vec![img_path.to_string_lossy().to_string()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };

        let content = build_api_content(&msg);
        match content {
            Some(ApiContent::Parts(parts)) => {
                assert_eq!(parts.len(), 2); // image + text
                // First part is ImageUrl
                match &parts[0] {
                    ApiContentPart::ImageUrl { image_url } => {
                        assert!(image_url.url.starts_with("data:image/png;base64,"));
                    }
                    _ => panic!("expected ImageUrl first"),
                }
                // Second part is text
                match &parts[1] {
                    ApiContentPart::Text { text } => assert_eq!(text, "describe this"),
                    _ => panic!("expected Text second"),
                }
            }
            other => panic!("expected Parts, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_provider_metadata() {
        let provider = OpenRouterProvider::new("test-key", "test-model");
        assert_eq!(provider.model_id(), "test-model");
        assert_eq!(provider.provider_name(), "openrouter");
    }

    #[test]
    fn test_with_base_url() {
        let provider =
            OpenRouterProvider::new("key", "model").with_base_url("http://localhost:8080");
        assert_eq!(provider.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_api_request_serialization() {
        let msg = ApiMessage {
            role: "user",
            content: Some(ApiContent::Text("hi".to_string())),
            tool_call_id: None,
            tool_calls: None,
        };
        let request = ApiRequest {
            model: "test",
            messages: vec![msg],
            max_tokens: Some(100),
            temperature: None,
            tools: None,
            tool_choice: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "test");
        assert_eq!(json["max_tokens"], 100);
        assert!(json.get("temperature").is_none());
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn test_api_request_with_tools() {
        let schema = serde_json::json!({"type": "object"});
        let tool = ApiTool {
            r#type: "function",
            function: ApiFunction {
                name: "test_fn",
                description: "A test",
                parameters: &schema,
            },
        };
        let request = ApiRequest {
            model: "m",
            messages: vec![],
            max_tokens: None,
            temperature: None,
            tool_choice: None,
            tools: Some(vec![tool]),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "test_fn");
    }

    #[test]
    fn prompt_cache_manifest_uses_final_openrouter_shape_and_redacts_content() {
        let provider = OpenRouterProvider::new("test-key", "test-model");
        let schema = serde_json::json!({"type": "object", "secret": "schema secret"});
        let tool = ApiTool {
            r#type: "function",
            function: ApiFunction {
                name: "private_tool",
                description: "tool description secret",
                parameters: &schema,
            },
        };
        let config = ChatConfig {
            prompt_cache_context: Some(PromptCacheContext {
                affinity_key: "session-affinity".into(),
                epoch_id: "epoch-7".into(),
                stable_prefix_hash: "canonical-hash".into(),
                semantic_boundaries: vec![],
            }),
            ..Default::default()
        };
        let first = ApiRequest {
            model: "test-model",
            messages: vec![
                ApiMessage {
                    role: "system",
                    content: Some(ApiContent::Text("system prompt secret".into())),
                    tool_call_id: None,
                    tool_calls: None,
                },
                ApiMessage {
                    role: "user",
                    content: Some(ApiContent::Text("first user secret".into())),
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            max_tokens: Some(128),
            temperature: Some(0.0),
            tools: Some(vec![tool]),
            tool_choice: None,
        };
        let first_manifest = provider.prompt_cache_input_manifest(&first, &config);

        let next = ApiRequest {
            model: "test-model",
            messages: vec![
                ApiMessage {
                    role: "system",
                    content: Some(ApiContent::Text("system prompt secret".into())),
                    tool_call_id: None,
                    tool_calls: None,
                },
                ApiMessage {
                    role: "user",
                    content: Some(ApiContent::Text("first user secret".into())),
                    tool_call_id: None,
                    tool_calls: None,
                },
                ApiMessage {
                    role: "assistant",
                    content: Some(ApiContent::Text("assistant suffix secret".into())),
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            max_tokens: Some(256),
            temperature: Some(0.7),
            tool_choice: None,
            tools: Some(vec![ApiTool {
                r#type: "function",
                function: ApiFunction {
                    name: "private_tool",
                    description: "tool description secret",
                    parameters: &schema,
                },
            }]),
        };
        let mut final_stream_body = serde_json::to_value(&next).unwrap();
        final_stream_body["stream"] = true.into();
        final_stream_body["stream_options"] = serde_json::json!({"include_usage": true});
        let next_manifest =
            provider.prompt_cache_input_manifest_from_value(&final_stream_body, &config);

        let comparison = first_manifest.compare_prefix(&next_manifest);
        assert!(comparison.compatible_route);
        assert!(comparison.stable_prefix_matches);
        assert_eq!(comparison.conversation_prefix_segments, 1);
        assert_eq!(first_manifest.epoch_id.as_deref(), Some("epoch-7"));
        assert_eq!(first_manifest.stable_segments.len(), 2);

        let redacted = serde_json::to_string(&next_manifest).unwrap();
        for secret in [
            "system prompt secret",
            "first user secret",
            "assistant suffix secret",
            "private_tool",
            "tool description secret",
            "schema secret",
        ] {
            assert!(!redacted.contains(secret));
        }
    }

    #[test]
    fn test_api_response_deserialization() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Hello!",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let resp: ApiResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello!"));
        assert_eq!(resp.choices[0].finish_reason, "stop");
        assert_eq!(resp.usage.prompt_tokens, 10);
        assert_eq!(resp.usage.completion_tokens, 5);
    }

    #[test]
    fn test_api_response_reasoning_field_to_chat_response() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Final answer.",
                    "reasoning": "OpenRouter reasoning text.",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let resp: ApiResponse = serde_json::from_value(json).unwrap();
        let chat = openrouter_response_to_chat_response(resp, "test-model").unwrap();
        assert_eq!(
            chat.reasoning_content.as_deref(),
            Some("OpenRouter reasoning text.")
        );
        assert_eq!(chat.content.as_deref(), Some("Final answer."));
    }

    #[test]
    fn test_api_response_reasoning_content_fallback_to_chat_response() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Final answer.",
                    "reasoning_content": "Compatible reasoning text.",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let resp: ApiResponse = serde_json::from_value(json).unwrap();
        let chat = openrouter_response_to_chat_response(resp, "test-model").unwrap();
        assert_eq!(
            chat.reasoning_content.as_deref(),
            Some("Compatible reasoning text.")
        );
        assert_eq!(chat.content.as_deref(), Some("Final answer."));
    }

    #[test]
    fn test_openrouter_sse_reasoning_delta_key() {
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"choices": [{"delta": {"reasoning": "Route-specific thought."}}]}"#.into(),
        };
        let events = crate::openai::parse_openai_sse_events(&event);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], crate::types::StreamEvent::ReasoningDelta(t) if t == "Route-specific thought.")
        );
    }

    #[test]
    fn test_api_response_with_tool_calls() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "function": {
                            "name": "search",
                            "arguments": "{\"query\":\"test\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 15,
                "completion_tokens": 8
            }
        });
        let resp: ApiResponse = serde_json::from_value(json).unwrap();
        let tc = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_123");
        assert_eq!(tc[0].function.name, "search");
        assert_eq!(tc[0].function.arguments, "{\"query\":\"test\"}");
    }
}

#[cfg(test)]
mod lane_attributed_operational_errors {
    use octos_core::Message;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::OpenRouterProvider;
    use crate::config::ChatConfig;
    use crate::provider::LlmProvider;
    use crate::provider::test_lanes::assert_error_names_lane;

    const LANE: &str = "openrouter/test-model";
    const STYLE: &str = "api_style=openrouter_chat_completions";
    const FORBIDDEN: &[&str] = &["OpenRouter response", "OpenRouter request"];

    async fn lane_returning(status: u16, body: &str) -> (MockServer, OpenRouterProvider) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body.to_owned()))
            .mount(&server)
            .await;
        let provider = OpenRouterProvider::new("key", "test-model").with_base_url(server.uri());
        (server, provider)
    }

    #[tokio::test]
    async fn should_name_lane_and_api_style_when_response_body_is_malformed() {
        let (_server, provider) = lane_returning(200, "not json{").await;
        let err = provider
            .chat(&[Message::user("hi")], &[], &ChatConfig::default())
            .await
            .unwrap_err();
        assert_error_names_lane(&err, LANE, STYLE, FORBIDDEN);
    }

    #[tokio::test]
    async fn should_name_lane_and_api_style_when_choices_are_empty() {
        let (_server, provider) = lane_returning(
            200,
            r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":0}}"#,
        )
        .await;
        let err = provider
            .chat(&[Message::user("hi")], &[], &ChatConfig::default())
            .await
            .unwrap_err();
        assert_error_names_lane(&err, LANE, STYLE, FORBIDDEN);
    }
}
