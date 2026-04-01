//! OpenAI (GPT) provider implementation.

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::{Message, MessageRole};

use reqwest::Client;
use serde::{Deserialize, Serialize};

use secrecy::{ExposeSecret, SecretString};

use crate::vision;

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::sse::SseEvent;
use crate::types::{ChatResponse, ChatStream, StopReason, StreamEvent, TokenUsage, ToolSpec};

/// Declarative hints about model API behavior.
///
/// Controls how requests are serialized for OpenAI-compatible endpoints.
/// By default, hints are auto-detected from the model name at construction time.
/// Users can override them via config for custom/unknown models.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelHints {
    /// Use `max_completion_tokens` instead of `max_tokens`.
    #[serde(default)]
    pub uses_completion_tokens: bool,

    /// Model does not support custom temperature.
    #[serde(default)]
    pub fixed_temperature: bool,

    /// Model lacks vision/multimodal support (images stripped from requests).
    #[serde(default)]
    pub lacks_vision: bool,

    /// Merge consecutive system messages into one (some providers reject multiples).
    #[serde(default = "default_true")]
    pub merge_system_messages: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ModelHints {
    fn default() -> Self {
        Self {
            uses_completion_tokens: false,
            fixed_temperature: false,
            lacks_vision: false,
            merge_system_messages: true,
        }
    }
}

impl ModelHints {
    /// Auto-detect hints from a model name string.
    ///
    /// This is the single canonical location for all model-name heuristics.
    /// Called once at provider construction time, not on every request.
    pub fn detect(model: &str) -> Self {
        let m = model.to_lowercase();

        let is_o_series = m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4");

        let uses_completion_tokens =
            is_o_series || m.starts_with("gpt-5") || m.starts_with("gpt-4.1");

        let fixed_temperature =
            is_o_series || m.starts_with("gpt-5") || m.contains("k2.5") || m == "gpt-4.1-nano";

        let lacks_vision = m.starts_with("deepseek")
            || m.starts_with("minimax")
            || m.contains("codestral")
            || m.starts_with("mistral")
            || m.starts_with("yi-");

        Self {
            uses_completion_tokens,
            fixed_temperature,
            lacks_vision,
            merge_system_messages: true,
        }
    }
}

/// OpenAI GPT provider.
pub struct OpenAIProvider {
    client: Client,
    api_key: SecretString,
    model: String,
    base_url: String,
    hints: ModelHints,
    /// Label for logs/failover. Defaults to `"openai"` but overridden by
    /// registry entries (e.g. `"moonshot"`, `"deepseek"`) so providers are
    /// distinguishable in failover chains.
    provider_label: String,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let model = model.into();
        let hints = ModelHints::detect(&model);
        Self {
            client: crate::provider::build_http_client(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
                crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            ),
            api_key: SecretString::from(api_key.into()),
            hints,
            model,
            base_url: "https://api.openai.com/v1".to_string(),
            provider_label: "openai".to_string(),
        }
    }

    /// Create a provider using the OPENAI_API_KEY environment variable.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .wrap_err("OPENAI_API_KEY environment variable not set")?;
        Ok(Self::new(api_key, "gpt-4o"))
    }

    /// Set a custom base URL (for Azure, local proxies, etc.).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override the auto-detected model hints.
    pub fn with_hints(mut self, hints: ModelHints) -> Self {
        self.hints = hints;
        self
    }

    /// Replace the HTTP client with one using custom timeouts (in seconds).
    pub fn with_http_timeout(mut self, timeout_secs: u64, connect_timeout_secs: u64) -> Self {
        self.client = crate::provider::build_http_client(timeout_secs, connect_timeout_secs);
        self
    }

    /// Set a custom provider label for logs and failover identification.
    /// By default this is `"openai"`, but registry entries override it
    /// (e.g. `"moonshot"`, `"deepseek"`) so providers are distinguishable.
    pub fn with_provider_label(mut self, label: impl Into<String>) -> Self {
        self.provider_label = label.into();
        self
    }

    /// Build the shared request struct used by both chat() and chat_stream().
    fn build_request<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
        config: &ChatConfig,
    ) -> OpenAIRequest<'a> {
        let openai_messages: Vec<OpenAIMessage> = messages
            .iter()
            .filter(|m| {
                // Drop empty assistant messages (no content, no tool_calls) —
                // these can appear in session history and cause 400 errors.
                !(m.role == MessageRole::Assistant
                    && m.content.is_empty()
                    && m.tool_calls.as_ref().is_none_or(|tc| tc.is_empty()))
            })
            .map(|m| {
                let role = m.role.as_str();
                // Convert tool_calls from octos_core format to OpenAI format
                let tool_calls = m.tool_calls.as_ref().map(|tcs| {
                    tcs.iter()
                        .map(|tc| OpenAIToolCall {
                            id: tc.id.clone(),
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name: tc.name.clone(),
                                arguments: tc.arguments.to_string(),
                            },
                        })
                        .collect()
                });
                // Kimi-k2.5 (and similar thinking models) require reasoning_content
                // to be present (even empty) on ALL assistant messages when thinking
                // is enabled. When omitted, the API returns 400 "reasoning_content
                // is missing in assistant tool call message".
                // Only synthesize a stub for models that actually need it (detected
                // via fixed_temperature + model name containing "k2.5").
                let needs_reasoning_stub =
                    self.hints.fixed_temperature && self.model.to_lowercase().contains("k2.5");
                let reasoning = match m.reasoning_content.as_deref() {
                    Some(r) if !r.is_empty() => Some(r),
                    _ if role == "assistant" && needs_reasoning_stub => Some("."),
                    _ => None,
                };

                OpenAIMessage {
                    role,
                    content: build_openai_content(m, &self.hints),
                    reasoning_content: reasoning,
                    tool_call_id: m.tool_call_id.as_deref(),
                    tool_calls,
                }
            })
            .collect();

        let openai_messages = if self.hints.merge_system_messages {
            merge_system_messages(openai_messages)
        } else {
            openai_messages
        };

        let openai_tools: Option<Vec<OpenAITool>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| OpenAITool {
                        r#type: "function",
                        function: OpenAIFunction {
                            name: &t.name,
                            description: &t.description,
                            parameters: &t.input_schema,
                        },
                    })
                    .collect(),
            )
        };

        let temperature = if self.hints.fixed_temperature {
            None
        } else {
            config.temperature
        };

        let response_format = config.response_format.as_ref().map(|rf| match rf {
            crate::config::ResponseFormat::Text => {
                serde_json::json!({"type": "text"})
            }
            crate::config::ResponseFormat::JsonObject => {
                serde_json::json!({"type": "json_object"})
            }
            crate::config::ResponseFormat::JsonSchema {
                name,
                schema,
                strict,
            } => {
                serde_json::json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": name,
                        "schema": schema,
                        "strict": strict,
                    }
                })
            }
        });

        OpenAIRequest {
            model: &self.model,
            messages: openai_messages,
            max_tokens: if self.hints.uses_completion_tokens {
                None
            } else {
                config.max_tokens
            },
            max_completion_tokens: if self.hints.uses_completion_tokens {
                config.max_tokens.or(Some(4096))
            } else {
                None
            },
            temperature,
            tools: openai_tools,
            response_format,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAIProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let request = self.build_request(messages, tools, config);

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
            ))
            .json(&request)
            .send()
            .await
            .wrap_err("failed to send request to OpenAI")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            eyre::bail!(
                "API error ({}): {status} - {}",
                self.model,
                crate::provider::truncate_error_body(&body)
            );
        }

        let api_response: OpenAIResponse = response
            .json()
            .await
            .wrap_err("failed to parse OpenAI response")?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("no choices in OpenAI response"))?;

        let tool_calls = choice
            .message
            .tool_calls
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

        // Strip <think> tags from content (DeepSeek, MiniMax, Qwen thinking models
        // embed chain-of-thought in <think> tags instead of reasoning_content).
        let (content, reasoning_content) = match choice.message.content {
            Some(text) => {
                let (cleaned, thinking) = crate::types::strip_think_tags(&text);
                let content = if cleaned.is_empty() {
                    None
                } else {
                    Some(cleaned)
                };
                // Prefer the structured reasoning_content if the provider sent one;
                // otherwise use what we extracted from <think> tags.
                let reasoning = choice.message.reasoning_content.or(thinking);
                (content, reasoning)
            }
            None => (None, choice.message.reasoning_content),
        };

        Ok(ChatResponse {
            content,
            reasoning_content,
            tool_calls,
            stop_reason,
            usage: TokenUsage {
                input_tokens: api_response.usage.prompt_tokens,
                output_tokens: api_response.usage.completion_tokens,
                ..Default::default()
            },
        })
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let request = self.build_request(messages, tools, config);

        let mut body =
            serde_json::to_value(&request).wrap_err("failed to serialize OpenAI request")?;
        let obj = body
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("failed to build OpenAI request body"))?;
        obj.insert("stream".into(), true.into());
        obj.insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send streaming request to OpenAI")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            eyre::bail!(
                "API error ({}): {status} - {}",
                self.model,
                crate::provider::truncate_error_body(&text)
            );
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
        &self.provider_label
    }
}

#[derive(Serialize)]
struct OpenAIRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAIMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct OpenAIMessage<'a> {
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OpenAIContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

/// Content can be plain text or multipart (text + images).
#[derive(Serialize)]
#[serde(untagged)]
enum OpenAIContent {
    Text(String),
    Parts(Vec<OpenAIContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum OpenAIContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: OpenAIImageUrl },
}

#[derive(Serialize)]
struct OpenAIImageUrl {
    url: String,
}

/// Merge consecutive system messages into a single system message.
///
/// Some OpenAI-compatible providers (e.g. MiniMax) reject requests with
/// multiple system messages. This combines their text content with a newline
/// separator while preserving all other messages in order.
fn merge_system_messages(messages: Vec<OpenAIMessage<'_>>) -> Vec<OpenAIMessage<'_>> {
    let mut result: Vec<OpenAIMessage> = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.role == "system" {
            if let Some(last) = result.last_mut() {
                if last.role == "system" {
                    // Merge content: extract text from both and combine
                    let existing = match &last.content {
                        Some(OpenAIContent::Text(t)) => t.clone(),
                        _ => String::new(),
                    };
                    let new_text = match &msg.content {
                        Some(OpenAIContent::Text(t)) => t.as_str(),
                        _ => "",
                    };
                    last.content = Some(OpenAIContent::Text(format!("{existing}\n\n{new_text}")));
                    continue;
                }
            }
        }
        result.push(msg);
    }
    result
}

fn build_openai_content(msg: &Message, hints: &ModelHints) -> Option<OpenAIContent> {
    let images: Vec<_> = if hints.lacks_vision {
        vec![]
    } else {
        msg.media.iter().filter(|p| vision::is_image(p)).collect()
    };

    if images.is_empty() {
        // If media was stripped due to model not supporting vision, note it in text
        let media_note = if hints.lacks_vision && msg.media.iter().any(|p| vision::is_image(p)) {
            let filenames: Vec<_> = msg
                .media
                .iter()
                .map(|p| {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone())
                })
                .collect();
            Some(format!("[attached media: {}]", filenames.join(", ")))
        } else {
            None
        };

        if msg.content.is_empty() && media_note.is_none() {
            // Tool messages require a content string (OpenAI spec).
            // User messages must not be empty (many providers reject them).
            // Assistant messages: some providers (Kimi, DeepSeek) reject omitted content,
            // NVIDIA NIM rejects empty string — use a single space as universal safe value.
            return match msg.role {
                MessageRole::Tool => Some(OpenAIContent::Text(String::new())),
                MessageRole::User => Some(OpenAIContent::Text("[empty message]".to_string())),
                MessageRole::Assistant => Some(OpenAIContent::Text(" ".to_string())),
                _ => None,
            };
        }
        let text = match media_note {
            Some(note) if msg.content.is_empty() => note,
            Some(note) => format!("{}\n{note}", msg.content),
            None => msg.content.clone(),
        };
        return Some(OpenAIContent::Text(text));
    }

    let mut parts = Vec::new();
    for path in images {
        if let Ok((mime, data)) = vision::encode_image(path) {
            parts.push(OpenAIContentPart::ImageUrl {
                image_url: OpenAIImageUrl {
                    url: format!("data:{mime};base64,{data}"),
                },
            });
        }
    }
    if !msg.content.is_empty() {
        parts.push(OpenAIContentPart::Text {
            text: msg.content.clone(),
        });
    }
    Some(OpenAIContent::Parts(parts))
}

#[derive(Serialize)]
struct OpenAITool<'a> {
    r#type: &'a str,
    function: OpenAIFunction<'a>,
}

#[derive(Serialize)]
struct OpenAIFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Deserialize)]
struct OpenAIResponse {
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
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

#[derive(Serialize, Deserialize)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type", default = "default_function_type")]
    call_type: String,
    function: FunctionCall,
}

fn default_function_type() -> String {
    "function".to_string()
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

// --- Streaming SSE helpers (shared with OpenRouter) ---

pub(crate) fn parse_openai_sse_events(event: &SseEvent) -> Vec<StreamEvent> {
    if event.data == "[DONE]" {
        return vec![];
    }

    // Detect error events from the SSE layer (network failures, etc.)
    if event.event.as_deref() == Some("error") {
        let msg = serde_json::from_str::<serde_json::Value>(&event.data)
            .ok()
            .and_then(|v| {
                v["error"]["message"]
                    .as_str()
                    .or_else(|| v["error"].as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| event.data.clone());
        return vec![StreamEvent::Error(msg)];
    }

    let data: serde_json::Value = match serde_json::from_str(&event.data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    // Provider-level error in JSON payload (e.g. DashScope {"error":{"message":"..."}})
    if let Some(err) = data.get("error") {
        let msg = err["message"]
            .as_str()
            .or_else(|| err.as_str())
            .unwrap_or("unknown error");
        return vec![StreamEvent::Error(msg.to_string())];
    }

    let mut events = Vec::new();

    if let Some(choices) = data["choices"].as_array() {
        for choice in choices {
            // Reasoning/thinking content (kimi-k2.5, o1, etc.)
            if let Some(reasoning) = choice["delta"]["reasoning_content"].as_str() {
                if !reasoning.is_empty() {
                    events.push(StreamEvent::ReasoningDelta(reasoning.to_string()));
                }
            }

            if let Some(content) = choice["delta"]["content"].as_str() {
                if !content.is_empty() {
                    events.push(StreamEvent::TextDelta(content.to_string()));
                }
            }

            if let Some(tool_calls) = choice["delta"]["tool_calls"].as_array() {
                for tc in tool_calls {
                    events.push(StreamEvent::ToolCallDelta {
                        index: tc["index"].as_u64().unwrap_or(0) as usize,
                        id: tc["id"].as_str().map(String::from),
                        name: tc["function"]["name"].as_str().map(String::from),
                        arguments_delta: tc["function"]["arguments"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    });
                }
            }

            if let Some(reason) = choice["finish_reason"].as_str() {
                events.push(StreamEvent::Done(match reason {
                    "stop" => StopReason::EndTurn,
                    "tool_calls" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    "content_filter" => StopReason::ContentFiltered,
                    _ => StopReason::EndTurn,
                }));
            }
        }
    }

    if let Some(usage) = data.get("usage").filter(|u| !u.is_null()) {
        events.push(StreamEvent::Usage(TokenUsage {
            input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0) as u32,
            ..Default::default()
        }));
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChatConfig;
    use crate::provider::LlmProvider;
    use octos_core::{Message, MessageRole};

    fn msg(content: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_detect_gpt4o() {
        let h = ModelHints::detect("gpt-4o");
        assert!(!h.uses_completion_tokens);
        assert!(!h.fixed_temperature);
        assert!(!h.lacks_vision);
    }

    #[test]
    fn test_detect_gpt4o_mini() {
        let h = ModelHints::detect("gpt-4o-mini");
        assert!(!h.uses_completion_tokens);
        assert!(!h.fixed_temperature);
    }

    #[test]
    fn test_detect_gpt41() {
        let h = ModelHints::detect("gpt-4.1");
        assert!(h.uses_completion_tokens);
        assert!(!h.fixed_temperature);
        assert!(!h.lacks_vision);
    }

    #[test]
    fn test_detect_gpt41_mini() {
        let h = ModelHints::detect("gpt-4.1-mini");
        assert!(h.uses_completion_tokens);
        assert!(!h.fixed_temperature);
    }

    #[test]
    fn test_detect_gpt5_uses_fixed_temperature() {
        // All gpt-5.* variants use fixed temperature and completion tokens
        for model in &["gpt-5-nano", "gpt-5.3-codex", "gpt-5.4"] {
            let h = ModelHints::detect(model);
            assert!(
                h.uses_completion_tokens,
                "{model} should use completion_tokens"
            );
            assert!(h.fixed_temperature, "{model} should use fixed_temperature");
        }
    }

    #[test]
    fn test_detect_o3() {
        let h = ModelHints::detect("o3-mini");
        assert!(h.uses_completion_tokens);
        assert!(h.fixed_temperature);
        assert!(!h.lacks_vision);
    }

    #[test]
    fn test_detect_o1() {
        let h = ModelHints::detect("o1-preview");
        assert!(h.uses_completion_tokens);
        assert!(h.fixed_temperature);
    }

    #[test]
    fn test_detect_kimi_k25() {
        let h = ModelHints::detect("kimi-k2.5");
        assert!(!h.uses_completion_tokens);
        assert!(h.fixed_temperature);
        assert!(!h.lacks_vision);
    }

    #[test]
    fn test_detect_deepseek() {
        let h = ModelHints::detect("deepseek-chat");
        assert!(!h.uses_completion_tokens);
        assert!(!h.fixed_temperature);
        assert!(h.lacks_vision);
    }

    #[test]
    fn test_detect_minimax() {
        let h = ModelHints::detect("MiniMax-Text-01");
        assert!(h.lacks_vision);
        assert!(h.merge_system_messages);
    }

    #[test]
    fn test_detect_unknown_model() {
        let h = ModelHints::detect("my-custom-model");
        assert!(!h.uses_completion_tokens);
        assert!(!h.fixed_temperature);
        assert!(!h.lacks_vision);
        assert!(h.merge_system_messages);
    }

    #[test]
    fn test_model_hints_serde_roundtrip() {
        let hints = ModelHints {
            uses_completion_tokens: true,
            fixed_temperature: false,
            lacks_vision: true,
            merge_system_messages: false,
        };
        let json = serde_json::to_string(&hints).unwrap();
        let parsed: ModelHints = serde_json::from_str(&json).unwrap();
        assert_eq!(hints, parsed);
    }

    #[test]
    fn test_model_hints_deserialize_partial() {
        let json = r#"{"uses_completion_tokens": true}"#;
        let h: ModelHints = serde_json::from_str(json).unwrap();
        assert!(h.uses_completion_tokens);
        assert!(!h.fixed_temperature);
        assert!(!h.lacks_vision);
        assert!(h.merge_system_messages);
    }

    #[test]
    fn test_with_hints_overrides_detection() {
        let p = OpenAIProvider::new("key", "gpt-4o").with_hints(ModelHints {
            uses_completion_tokens: true,
            fixed_temperature: true,
            lacks_vision: true,
            merge_system_messages: false,
        });
        assert!(p.hints.uses_completion_tokens);
        assert!(p.hints.fixed_temperature);
        assert!(p.hints.lacks_vision);
        assert!(!p.hints.merge_system_messages);
    }

    /// Real API test: NVIDIA NIM with Llama 3.3 70B.
    /// Run with: NVIDIA_API_KEY=... cargo test -p octos-llm -- --ignored test_nvidia_nim_llama
    #[tokio::test]
    #[ignore]
    async fn test_nvidia_nim_llama() {
        let api_key = std::env::var("NVIDIA_API_KEY").expect("NVIDIA_API_KEY must be set");
        let provider = OpenAIProvider::new(&api_key, "meta/llama-3.3-70b-instruct")
            .with_base_url("https://integrate.api.nvidia.com/v1");

        assert_eq!(provider.model_id(), "meta/llama-3.3-70b-instruct");

        let messages = vec![msg("What is 2+2? Reply with just the number.")];
        let config = ChatConfig {
            max_tokens: Some(64),
            ..Default::default()
        };
        let response = provider.chat(&messages, &[], &config).await.unwrap();

        eprintln!("NVIDIA Llama response: {:?}", response.content);
        eprintln!("Tokens: {:?}", response.usage);

        assert!(response.content.is_some());
        let content = response.content.unwrap();
        assert!(content.contains('4'), "Expected '4' in response: {content}");
        assert!(response.usage.input_tokens > 0);
        assert!(response.usage.output_tokens > 0);
    }

    /// Real API test: NVIDIA NIM with Mistral Small.
    /// Run with: NVIDIA_API_KEY=... cargo test -p octos-llm -- --ignored test_nvidia_nim_mistral
    #[tokio::test]
    #[ignore]
    async fn test_nvidia_nim_mistral() {
        let api_key = std::env::var("NVIDIA_API_KEY").expect("NVIDIA_API_KEY must be set");
        let provider =
            OpenAIProvider::new(&api_key, "mistralai/mistral-small-3.1-24b-instruct-2503")
                .with_base_url("https://integrate.api.nvidia.com/v1");

        let messages = vec![msg("Name the capital of France in one word.")];
        let config = ChatConfig {
            max_tokens: Some(32),
            ..Default::default()
        };
        let response = provider.chat(&messages, &[], &config).await.unwrap();

        eprintln!("NVIDIA Mistral response: {:?}", response.content);
        let content = response.content.unwrap();
        assert!(
            content.to_lowercase().contains("paris"),
            "Expected 'Paris' in response: {content}"
        );
    }

    /// Real API test: NVIDIA NIM streaming.
    /// Run with: NVIDIA_API_KEY=... cargo test -p octos-llm -- --ignored test_nvidia_nim_streaming
    #[tokio::test]
    #[ignore]
    async fn test_nvidia_nim_streaming() {
        let api_key = std::env::var("NVIDIA_API_KEY").expect("NVIDIA_API_KEY must be set");
        let provider = OpenAIProvider::new(&api_key, "meta/llama-3.3-70b-instruct")
            .with_base_url("https://integrate.api.nvidia.com/v1");

        let messages = vec![msg("Count from 1 to 5, one number per line.")];
        let config = ChatConfig {
            max_tokens: Some(64),
            ..Default::default()
        };
        let mut stream = provider.chat_stream(&messages, &[], &config).await.unwrap();

        let mut chunks = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::TextDelta(text) => chunks.push(text),
                StreamEvent::Done(_) => break,
                _ => {}
            }
        }

        let full_text = chunks.join("");
        eprintln!("NVIDIA streaming result: {full_text}");
        assert!(!full_text.is_empty(), "Stream should produce text");
        assert!(full_text.contains('1'), "Should contain '1': {full_text}");
        assert!(full_text.contains('5'), "Should contain '5': {full_text}");
    }
}
