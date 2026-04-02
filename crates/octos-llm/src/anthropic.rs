//! Anthropic (Claude) provider implementation.

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::Message;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use secrecy::{ExposeSecret, SecretString};

use crate::vision;

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, StopReason, StreamEvent, TokenUsage, ToolSpec};

/// Anthropic Claude provider.
pub struct AnthropicProvider {
    client: Client,
    api_key: SecretString,
    model: String,
    base_url: String,
    /// Label for logs/failover. Defaults to `"anthropic"` but overridden by
    /// registry entries (e.g. `"zai"`, `"r9s"`) so providers are
    /// distinguishable in failover chains.
    provider_label: String,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::provider::build_http_client(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
                crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            ),
            api_key: SecretString::from(api_key.into()),
            model: model.into(),
            base_url: "https://api.anthropic.com".to_string(),
            provider_label: "anthropic".to_string(),
        }
    }

    /// Create a provider using the ANTHROPIC_API_KEY environment variable.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .wrap_err("ANTHROPIC_API_KEY environment variable not set")?;
        Ok(Self::new(api_key, "claude-sonnet-4-20250514"))
    }

    /// Set a custom base URL (for compatible endpoints).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Replace the HTTP client with one using custom timeouts (in seconds).
    pub fn with_http_timeout(mut self, timeout_secs: u64, connect_timeout_secs: u64) -> Self {
        self.client = crate::provider::build_http_client(timeout_secs, connect_timeout_secs);
        self
    }

    /// Override the provider label shown in logs and status display.
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
    ) -> AnthropicRequest<'a> {
        AnthropicRequest {
            model: &self.model,
            max_tokens: config.max_tokens.unwrap_or(4096),
            messages: messages
                .iter()
                .filter(|m| m.role != octos_core::MessageRole::System)
                .map(|m| {
                    let role = match m.role {
                        octos_core::MessageRole::User => "user",
                        octos_core::MessageRole::Assistant => "assistant",
                        octos_core::MessageRole::Tool => "user",
                        octos_core::MessageRole::System => "user",
                    };
                    AnthropicMessage {
                        role,
                        content: build_anthropic_content(m),
                    }
                })
                .collect(),
            system: {
                let system_parts: Vec<&str> = messages
                    .iter()
                    .filter(|m| m.role == octos_core::MessageRole::System)
                    .map(|m| m.content.as_str())
                    .collect();
                if system_parts.is_empty() {
                    None
                } else {
                    Some(system_parts.join("\n\n"))
                }
            },
            tools: if tools.is_empty() { None } else { Some(tools) },
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let request = self.build_request(messages, tools, config);

        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .timeout(std::time::Duration::from_secs(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
            ))
            .json(&request)
            .send()
            .await
            .wrap_err("failed to send request to Anthropic")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            eyre::bail!(
                "Anthropic API error: {status} - {}",
                crate::provider::truncate_error_body(&body)
            );
        }

        let api_response: AnthropicResponse = response
            .json()
            .await
            .wrap_err("failed to parse Anthropic response")?;

        // Convert response to our types
        let mut content = None;
        let mut tool_calls = Vec::new();

        for block in api_response.content {
            match block {
                ContentBlock::Text { text } => {
                    content = Some(text);
                }
                ContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(octos_core::ToolCall {
                        id,
                        name,
                        arguments: input,
                        metadata: None,
                    });
                }
            }
        }

        let stop_reason = match api_response.stop_reason.as_str() {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };

        Ok(ChatResponse {
            content,
            reasoning_content: None,
            tool_calls,
            stop_reason,
            usage: TokenUsage {
                input_tokens: api_response.usage.input_tokens,
                output_tokens: api_response.usage.output_tokens,
                ..Default::default()
            },
            provider_index: None,
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
            serde_json::to_value(&request).wrap_err("failed to serialize Anthropic request")?;
        body.as_object_mut()
            .ok_or_else(|| eyre::eyre!("failed to build Anthropic request body"))?
            .insert("stream".into(), true.into());

        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send streaming request to Anthropic")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            eyre::bail!(
                "Anthropic API error: {status} - {}",
                crate::provider::truncate_error_body(&text)
            );
        }

        let sse_stream = crate::sse::parse_sse_response(response);
        let state = AnthropicStreamState::default();
        let event_stream = sse_stream
            .scan(state, |state, event| {
                let events = map_anthropic_sse(state, &event);
                futures::future::ready(Some(events))
            })
            .flat_map(futures::stream::iter);

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
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolSpec]>,
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: AnthropicContent,
}

/// Content can be plain text or multipart (text + images).
#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicContent {
    Text(String),
    Parts(Vec<AnthropicContentBlock>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: AnthropicImageSource },
}

#[derive(Serialize)]
struct AnthropicImageSource {
    r#type: String,
    media_type: String,
    data: String,
}

fn build_anthropic_content(msg: &Message) -> AnthropicContent {
    let images: Vec<_> = msg.media.iter().filter(|p| vision::is_image(p)).collect();

    if images.is_empty() {
        // Include non-image file paths so the agent can use read_file
        let non_image: Vec<_> = msg.media.iter().filter(|p| !vision::is_image(p)).collect();
        if non_image.is_empty() {
            return AnthropicContent::Text(msg.content.clone());
        }
        let note = format!(
            "[attached files: {}. Use read_file to access them.]",
            non_image.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ")
        );
        let text = if msg.content.is_empty() {
            note
        } else {
            format!("{}\n{note}", msg.content)
        };
        return AnthropicContent::Text(text);
    }

    let mut parts = Vec::new();
    for path in images {
        if let Ok((mime, data)) = vision::encode_image(path) {
            parts.push(AnthropicContentBlock::Image {
                source: AnthropicImageSource {
                    r#type: "base64".into(),
                    media_type: mime,
                    data,
                },
            });
        }
    }
    if !msg.content.is_empty() {
        parts.push(AnthropicContentBlock::Text {
            text: msg.content.clone(),
        });
    }
    AnthropicContent::Parts(parts)
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
    stop_reason: String,
    usage: ApiUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// --- Streaming SSE helpers ---

#[derive(Default)]
struct AnthropicStreamState {
    block_to_tool: std::collections::HashMap<usize, usize>,
    tool_count: usize,
    input_tokens: u32,
}

// Visible for testing
fn map_anthropic_sse(
    state: &mut AnthropicStreamState,
    event: &crate::sse::SseEvent,
) -> Vec<StreamEvent> {
    // Handle SSE-level error events (e.g. Z.AI returns `event: error` with HTTP 200)
    if event.event.as_deref() == Some("error") {
        let msg = match serde_json::from_str::<serde_json::Value>(&event.data) {
            Ok(v) => v["error"]["message"]
                .as_str()
                .unwrap_or(&event.data)
                .to_string(),
            Err(_) => event.data.clone(),
        };
        return vec![StreamEvent::Error(msg)];
    }

    let data: serde_json::Value = match serde_json::from_str(&event.data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    // Handle error payloads without SSE event type (fallback)
    if data.get("error").is_some() {
        let msg = data["error"]["message"]
            .as_str()
            .unwrap_or("unknown API error")
            .to_string();
        return vec![StreamEvent::Error(msg)];
    }

    match data["type"].as_str().unwrap_or("") {
        "message_start" => {
            if let Some(t) = data["message"]["usage"]["input_tokens"].as_u64() {
                state.input_tokens = t as u32;
            }
            vec![]
        }
        "content_block_start" => {
            let idx = data["index"].as_u64().unwrap_or(0) as usize;
            if data["content_block"]["type"].as_str() == Some("tool_use") {
                let tool_idx = state.tool_count;
                state.tool_count += 1;
                state.block_to_tool.insert(idx, tool_idx);
                vec![StreamEvent::ToolCallDelta {
                    index: tool_idx,
                    id: data["content_block"]["id"].as_str().map(String::from),
                    name: data["content_block"]["name"].as_str().map(String::from),
                    arguments_delta: String::new(),
                }]
            } else {
                vec![]
            }
        }
        "content_block_delta" => {
            let idx = data["index"].as_u64().unwrap_or(0) as usize;
            match data["delta"]["type"].as_str().unwrap_or("") {
                "text_delta" => {
                    vec![StreamEvent::TextDelta(
                        data["delta"]["text"].as_str().unwrap_or("").to_string(),
                    )]
                }
                "input_json_delta" => {
                    if let Some(&tool_idx) = state.block_to_tool.get(&idx) {
                        vec![StreamEvent::ToolCallDelta {
                            index: tool_idx,
                            id: None,
                            name: None,
                            arguments_delta: data["delta"]["partial_json"]
                                .as_str()
                                .unwrap_or("")
                                .to_string(),
                        }]
                    } else {
                        vec![]
                    }
                }
                _ => vec![],
            }
        }
        "message_delta" => {
            let stop_reason = match data["delta"]["stop_reason"].as_str() {
                Some("end_turn") => StopReason::EndTurn,
                Some("tool_use") => StopReason::ToolUse,
                Some("max_tokens") => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            };
            let output_tokens = data["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32;
            // Some providers (Z.AI) report input_tokens in message_delta instead of
            // message_start. Use the delta value if it's non-zero.
            if let Some(t) = data["usage"]["input_tokens"].as_u64() {
                if t > 0 {
                    state.input_tokens = t as u32;
                }
            }
            vec![
                StreamEvent::Usage(TokenUsage {
                    input_tokens: state.input_tokens,
                    output_tokens,
                    ..Default::default()
                }),
                StreamEvent::Done(stop_reason),
            ]
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::{Message, MessageRole};

    fn msg(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    // --- build_anthropic_content tests ---

    #[test]
    fn test_build_content_text_only() {
        let m = msg(MessageRole::User, "hello");
        let content = build_anthropic_content(&m);
        match content {
            AnthropicContent::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("expected Text variant"),
        }
    }

    #[test]
    fn test_build_content_with_non_image_media() {
        let m = Message {
            role: MessageRole::User,
            content: "check this".into(),
            media: vec!["file.txt".into(), "data.csv".into()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };
        // Non-image media should include file paths for read_file
        let content = build_anthropic_content(&m);
        match content {
            AnthropicContent::Text(t) => {
                assert!(t.contains("check this"));
                assert!(t.contains("file.txt"));
                assert!(t.contains("data.csv"));
                assert!(t.contains("read_file"));
            }
            _ => panic!("expected Text for non-image media"),
        }
    }

    // --- build_request tests ---

    #[test]
    fn test_build_request_filters_system() {
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![
            msg(MessageRole::System, "system prompt"),
            msg(MessageRole::User, "hello"),
            msg(MessageRole::Assistant, "hi"),
        ];
        let config = ChatConfig::default();
        let request = provider.build_request(&messages, &[], &config);

        // System message should be extracted, not in messages array
        assert_eq!(request.system, Some("system prompt".to_string()));
        assert_eq!(request.messages.len(), 2); // user + assistant only
        assert_eq!(request.messages[0].role, "user");
        assert_eq!(request.messages[1].role, "assistant");
    }

    #[test]
    fn test_build_request_tool_role_mapped_to_user() {
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![Message {
            role: MessageRole::Tool,
            content: "tool result".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("tc1".into()),
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }];
        let config = ChatConfig::default();
        let request = provider.build_request(&messages, &[], &config);

        assert_eq!(request.messages[0].role, "user");
    }

    #[test]
    fn test_build_request_tools_none_when_empty() {
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![msg(MessageRole::User, "hi")];
        let config = ChatConfig::default();
        let request = provider.build_request(&messages, &[], &config);
        assert!(request.tools.is_none());
    }

    #[test]
    fn test_build_request_default_max_tokens() {
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![msg(MessageRole::User, "hi")];
        let config = ChatConfig::default();
        let request = provider.build_request(&messages, &[], &config);
        assert_eq!(request.max_tokens, 8192);
    }

    // --- SSE mapping tests ---

    #[test]
    fn test_sse_message_start() {
        let mut state = AnthropicStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"type": "message_start", "message": {"usage": {"input_tokens": 42}}}"#.into(),
        };
        let events = map_anthropic_sse(&mut state, &event);
        assert!(events.is_empty());
        assert_eq!(state.input_tokens, 42);
    }

    #[test]
    fn test_sse_text_delta() {
        let mut state = AnthropicStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}"#.into(),
        };
        let events = map_anthropic_sse(&mut state, &event);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "Hello"));
    }

    #[test]
    fn test_sse_tool_call_start() {
        let mut state = AnthropicStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "tc1", "name": "shell"}}"#.into(),
        };
        let events = map_anthropic_sse(&mut state, &event);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ToolCallDelta {
                index, id, name, ..
            } => {
                assert_eq!(*index, 0);
                assert_eq!(id.as_deref(), Some("tc1"));
                assert_eq!(name.as_deref(), Some("shell"));
            }
            _ => panic!("expected ToolCallDelta"),
        }
        assert_eq!(state.tool_count, 1);
    }

    #[test]
    fn test_sse_message_delta_end_turn() {
        let mut state = AnthropicStreamState::default();
        state.input_tokens = 100;
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 50}}"#.into(),
        };
        let events = map_anthropic_sse(&mut state, &event);
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], StreamEvent::Usage(u) if u.input_tokens == 100 && u.output_tokens == 50)
        );
        assert!(matches!(&events[1], StreamEvent::Done(StopReason::EndTurn)));
    }

    #[test]
    fn test_sse_error_event() {
        let mut state = AnthropicStreamState::default();
        let event = crate::sse::SseEvent {
            event: Some("error".into()),
            data: r#"{"error": {"message": "rate limited"}}"#.into(),
        };
        let events = map_anthropic_sse(&mut state, &event);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::Error(msg) if msg == "rate limited"));
    }

    #[test]
    fn test_sse_invalid_json_returns_empty() {
        let mut state = AnthropicStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: "not json".into(),
        };
        let events = map_anthropic_sse(&mut state, &event);
        assert!(events.is_empty());
    }

    // --- Provider metadata tests ---

    #[test]
    fn test_provider_name_and_model() {
        let provider = AnthropicProvider::new("test-key", "claude-3-haiku");
        assert_eq!(provider.provider_name(), "anthropic");
        assert_eq!(provider.model_id(), "claude-3-haiku");
    }

    #[test]
    fn test_with_base_url() {
        let provider =
            AnthropicProvider::new("key", "model").with_base_url("https://custom.api.com");
        assert_eq!(provider.base_url, "https://custom.api.com");
    }
}
