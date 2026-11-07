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
        let scope_root = config.media_scope_root.as_deref();
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .map(|m| build_api_message(m, scope_root))
            .collect();

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
        let scope_root = config.media_scope_root.as_deref();
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .map(|m| build_api_message(m, scope_root))
            .collect();

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

fn build_api_message<'a>(msg: &'a Message, scope_root: Option<&std::path::Path>) -> ApiMessage<'a> {
    let role = msg.role.as_str();
    let content = build_api_content(msg, scope_root);
    let tool_calls = msg.tool_calls.as_ref().map(|tcs| {
        tcs.iter()
            .map(|tc| ApiToolCall {
                id: tc.id.clone(),
                call_type: default_tool_call_type(),
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

fn build_api_content(msg: &Message, scope_root: Option<&std::path::Path>) -> Option<ApiContent> {
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
        if let Ok((mime, data)) = vision::encode_image(path, scope_root) {
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
    /// `"function"` on every call. Required by the OpenAI chat schema and
    /// enforced by some upstreams behind OpenRouter: Z.AI rejects a replayed
    /// assistant `tool_calls` entry without it (`1214 Tool type cannot be
    /// empty`), which killed every agent turn on `z-ai/*` models at the
    /// first tool result. Defaulted on deserialize for upstreams that omit it.
    #[serde(rename = "type", default = "default_tool_call_type")]
    call_type: String,
    function: FunctionCall,
}

fn default_tool_call_type() -> String {
    "function".to_owned()
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
    /// Automatic prompt-cache breakdown. `cached_tokens` counts the portion
    /// of `prompt_tokens` served from the upstream provider's cache
    /// (INCLUDED in `prompt_tokens`, unlike Anthropic's disjoint accounting).
    /// Upstream providers that omit the object parse as `None`.
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
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

    let truncated = choice.finish_reason == "length";
    let tool_calls = tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|tc| {
            let call_id = tc.id;
            let name = tc.function.name;
            let arguments = serde_json::from_str(&tc.function.arguments).map_err(|error| {
                let detail = format!(
                    "{} malformed tool arguments: {error}",
                    crate::provider::operational_error_message(
                        crate::provider::OperationalStage::ParseResponse,
                        "openrouter",
                        model,
                        crate::provider::ApiStyle::OpenRouterChatCompletions,
                    )
                );
                if truncated {
                    eyre::Report::new(crate::error::StreamError::TruncatedToolCall {
                        tool_id: call_id.clone(),
                        tool_name: name.clone(),
                        error: detail,
                    })
                } else {
                    eyre::Report::new(crate::error::StreamError::MalformedArgs {
                        tool_id: call_id.clone(),
                        tool_name: name.clone(),
                        error: detail,
                    })
                }
            })?;
            Ok(octos_core::ToolCall {
                id: call_id,
                name,
                arguments,
                metadata: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;

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
        usage: {
            // OpenRouter reports cached tokens INSIDE prompt_tokens; the
            // TokenUsage contract is disjoint (Anthropic-style: total
            // prompt = input + cache_read), so subtract at the boundary.
            // Same normalization as the shared SSE parse — a cache hit must
            // not report different usage depending on the stream flag.
            let cached = usage
                .prompt_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens)
                .unwrap_or(0);
            TokenUsage {
                input_tokens: usage.prompt_tokens.saturating_sub(cached),
                output_tokens: usage.completion_tokens,
                cache_read_tokens: cached,
                ..Default::default()
            }
        },
        provider_index: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            source: None,
            participant: None,
        }
    }

    #[test]
    fn replayed_assistant_tool_calls_carry_the_function_type() {
        // Z.AI behind OpenRouter rejects a tool_calls entry without
        // `"type":"function"` (error 1214 "Tool type cannot be empty"), which
        // failed every agent turn on `z-ai/*` models at the first tool result.
        let mut msg = text_msg(MessageRole::Assistant, "");
        msg.tool_calls = Some(vec![octos_core::ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
            metadata: None,
        }]);
        let json = serde_json::to_value(build_api_message(&msg, None)).expect("serialises");
        assert_eq!(json["tool_calls"][0]["type"], "function");
        assert_eq!(json["tool_calls"][0]["id"], "call_1");
        assert_eq!(json["tool_calls"][0]["function"]["name"], "read_file");

        // Upstreams that omit the field on the way back still parse.
        let parsed: ApiToolCall = serde_json::from_value(serde_json::json!({
            "id": "call_2",
            "function": {"name": "shell", "arguments": "{}"}
        }))
        .expect("type defaults");
        assert_eq!(parsed.call_type, "function");
    }

    #[test]
    fn test_api_response_cached_tokens_normalized_to_disjoint_accounting() {
        // OpenRouter reports the cached portion INSIDE prompt_tokens; the
        // TokenUsage contract is disjoint, so the total prompt is
        // input + cache_read. The non-streaming path must normalize exactly
        // like the shared SSE parse does (a cache hit must not change the
        // reported usage with the stream flag).
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Hello!",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 5,
                "prompt_tokens_details": { "cached_tokens": 75 }
            }
        });
        let resp: ApiResponse = serde_json::from_value(json).unwrap();
        let chat = openrouter_response_to_chat_response(resp, "test-model").unwrap();
        assert_eq!(chat.usage.input_tokens, 25);
        assert_eq!(chat.usage.cache_read_tokens, 75);
    }

    #[test]
    fn should_surface_invalid_json_through_openrouter_sse_mapper() {
        let event = crate::sse::SseEvent {
            event: None,
            data: "{not-json".into(),
        };
        let events = crate::openai::parse_openai_sse_events(&event);
        assert!(
            matches!(events.as_slice(), [crate::types::StreamEvent::Error(msg)]
                if msg.contains("OpenAI-compatible SSE JSON") && msg.contains("key must be a string")),
            "{events:?}"
        );
    }
}
