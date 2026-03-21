//! OpenAI Responses API provider (`POST /v1/responses`).
//!
//! Supports reasoning token breakdowns, structured output, and native
//! OpenAI tools. Falls back gracefully — the registry selects this
//! provider only for actual OpenAI endpoints with capable models.

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::{Message, MessageRole};

use reqwest::Client;
use serde::Deserialize;

use secrecy::{ExposeSecret, SecretString};

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, StopReason, StreamEvent, TokenUsage, ToolSpec};

/// OpenAI provider using the Responses API.
pub struct OpenAIResponsesProvider {
    client: Client,
    api_key: SecretString,
    model: String,
    base_url: String,
}

impl OpenAIResponsesProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::provider::build_http_client(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
                crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            ),
            api_key: SecretString::from(api_key.into()),
            model: model.into(),
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_http_timeout(mut self, timeout_secs: u64, connect_timeout_secs: u64) -> Self {
        self.client = crate::provider::build_http_client(timeout_secs, connect_timeout_secs);
        self
    }

    fn build_request(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> serde_json::Value {
        let input = build_input_messages(messages);

        let mut body = serde_json::json!({
            "model": &self.model,
            "input": input,
        });

        if let Some(max) = config.max_tokens {
            body["max_output_tokens"] = max.into();
        }

        if !tools.is_empty() {
            let api_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "name": &t.name,
                        "description": &t.description,
                        "parameters": &t.input_schema,
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(api_tools);
        }

        // Reasoning effort maps to the reasoning object
        if let Some(effort) = &config.reasoning_effort {
            let effort_str = match effort {
                crate::config::ReasoningEffort::Low => "low",
                crate::config::ReasoningEffort::Medium => "medium",
                crate::config::ReasoningEffort::High => "high",
            };
            body["reasoning"] = serde_json::json!({
                "effort": effort_str,
            });
        }

        body
    }
}

#[async_trait]
impl LlmProvider for OpenAIResponsesProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let body = self.build_request(messages, tools, config);

        let response = self
            .client
            .post(format!("{}/responses", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send request to OpenAI Responses API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            eyre::bail!(
                "OpenAI Responses API error: {status} - {}",
                crate::provider::truncate_error_body(&body)
            );
        }

        let api_response: ResponsesApiResponse = response
            .json()
            .await
            .wrap_err("failed to parse OpenAI Responses API response")?;

        Ok(parse_responses_api(api_response))
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let mut body = self.build_request(messages, tools, config);
        body["stream"] = true.into();

        let response = self
            .client
            .post(format!("{}/responses", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send streaming request to OpenAI Responses API")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            eyre::bail!(
                "OpenAI Responses API error: {status} - {}",
                crate::provider::truncate_error_body(&text)
            );
        }

        let sse_stream = crate::sse::parse_sse_response(response);
        let state = ResponsesStreamState::default();
        let event_stream = sse_stream
            .scan(state, |state, event| {
                let events = map_responses_sse(state, &event);
                futures::future::ready(Some(events))
            })
            .flat_map(futures::stream::iter);

        Ok(Box::pin(event_stream))
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "openai"
    }
}

// ---- Input message building ----

fn build_input_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut input = Vec::new();
    for msg in messages {
        // Skip empty assistant messages with no tool calls
        if msg.role == MessageRole::Assistant
            && msg.content.is_empty()
            && msg.tool_calls.as_ref().is_none_or(|tc| tc.is_empty())
        {
            continue;
        }
        build_input_items(msg, &mut input);
    }
    input
}

/// Normalize a tool_call_id for the OpenAI Responses API.
///
/// The Responses API requires IDs to begin with `fc_` (not `call_`).
/// The agent's message_repair normalizes all IDs to `call_` prefix for
/// Chat Completions compatibility. This second stage rewrites `call_` → `fc_`
/// specifically for the Responses API format.
fn normalize_call_id(id: &str) -> String {
    // Responses API requires `fc_` prefix on function_call item IDs.
    // `call_` prefix works for Chat Completions but NOT Responses API.
    if id.starts_with("fc_") {
        return id.to_string();
    }
    // Strip any existing prefix and re-prefix with `fc_`
    let stripped = id
        .strip_prefix("call_")
        .or_else(|| id.strip_prefix("call_function_"))
        .or_else(|| id.strip_prefix("toolu_"))
        .or_else(|| id.strip_prefix("chatcmpl-"))
        .unwrap_or(id);
    format!("fc_{stripped}")
}

/// Append one or more Responses API input items for a message.
///
/// The Responses API requires `function_call` to be a top-level input item,
/// NOT nested inside an assistant message's content array (which only accepts
/// `output_text` and `refusal`). So an assistant message with tool calls is
/// split into: an assistant message (text only) + separate function_call items.
///
/// All tool_call_ids are normalized to `call_` prefix for Responses API compat.
fn build_input_items(msg: &Message, out: &mut Vec<serde_json::Value>) {
    match msg.role {
        MessageRole::System => {
            out.push(serde_json::json!({
                "role": "system",
                "content": &msg.content,
            }));
        }
        MessageRole::User => {
            out.push(serde_json::json!({
                "role": "user",
                "content": build_user_content(msg),
            }));
        }
        MessageRole::Assistant => {
            // Emit assistant text content (if any)
            if !msg.content.is_empty() {
                out.push(serde_json::json!({
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": &msg.content }],
                }));
            }
            // Emit each tool call as a top-level function_call item
            if let Some(tool_calls) = &msg.tool_calls {
                for tc in tool_calls {
                    let cid = normalize_call_id(&tc.id);
                    out.push(serde_json::json!({
                        "type": "function_call",
                        "id": &cid,
                        "call_id": &cid,
                        "name": &tc.name,
                        "arguments": tc.arguments.to_string(),
                    }));
                }
            }
        }
        MessageRole::Tool => {
            let raw_id = msg.tool_call_id.as_deref().unwrap_or("unknown");
            let call_id = normalize_call_id(raw_id);
            out.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": &msg.content,
            }));
        }
    }
}

fn build_user_content(msg: &Message) -> serde_json::Value {
    let images: Vec<_> = msg
        .media
        .iter()
        .filter(|p| crate::vision::is_image(p))
        .collect();

    if images.is_empty() {
        return serde_json::json!([{ "type": "input_text", "text": &msg.content }]);
    }

    let mut parts = Vec::new();
    for path in &images {
        if let Ok((mime, data)) = crate::vision::encode_image(path) {
            parts.push(serde_json::json!({
                "type": "input_image",
                "image_url": format!("data:{mime};base64,{data}"),
            }));
        }
    }
    if !msg.content.is_empty() {
        parts.push(serde_json::json!({
            "type": "input_text",
            "text": &msg.content,
        }));
    }
    serde_json::Value::Array(parts)
}

// ---- Response parsing ----

#[derive(Deserialize)]
struct ResponsesApiResponse {
    output: Vec<OutputItem>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    usage: ResponsesUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputItem {
    Message {
        content: Vec<ContentPart>,
    },
    FunctionCall {
        id: String,
        #[serde(default)]
        call_id: String,
        name: String,
        arguments: String,
    },
    Reasoning {
        #[serde(default)]
        content: Vec<ReasoningPart>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    OutputText { text: String },
    Refusal { refusal: String },
}

#[derive(Deserialize)]
struct ReasoningPart {
    #[serde(default)]
    text: String,
}

#[derive(Default, Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Deserialize)]
struct OutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

fn parse_responses_api(resp: ResponsesApiResponse) -> ChatResponse {
    let mut content = None;
    let mut reasoning_content = None;
    let mut tool_calls = Vec::new();

    for item in resp.output {
        match item {
            OutputItem::Message { content: parts } => {
                for part in parts {
                    match part {
                        ContentPart::OutputText { text } => {
                            content = Some(text);
                        }
                        ContentPart::Refusal { refusal } => {
                            content = Some(format!("[Refusal] {refusal}"));
                        }
                    }
                }
            }
            OutputItem::FunctionCall {
                id,
                call_id,
                name,
                arguments,
            } => {
                let call_id = if call_id.is_empty() { id } else { call_id };
                let parsed_args = match serde_json::from_str(&arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            call_id = %call_id,
                            name = %name,
                            "failed to parse tool call arguments: {e}"
                        );
                        serde_json::Value::Null
                    }
                };
                tool_calls.push(octos_core::ToolCall {
                    id: call_id,
                    name,
                    arguments: parsed_args,
                    metadata: None,
                });
            }
            OutputItem::Reasoning { content: parts } => {
                let text: String = parts
                    .into_iter()
                    .map(|p| p.text)
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    reasoning_content = Some(text);
                }
            }
        }
    }

    let stop_reason = if !tool_calls.is_empty() {
        StopReason::ToolUse
    } else {
        match resp.status.as_str() {
            "completed" => StopReason::EndTurn,
            "incomplete" => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        }
    };

    let reasoning_tokens = resp
        .usage
        .output_tokens_details
        .map(|d| d.reasoning_tokens)
        .unwrap_or(0);

    ChatResponse {
        content,
        reasoning_content,
        tool_calls,
        stop_reason,
        usage: TokenUsage {
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
            reasoning_tokens,
            ..Default::default()
        },
    }
}

// ---- Streaming SSE ----

#[derive(Default)]
struct ResponsesStreamState {
    tool_calls: Vec<(String, String, String)>, // (call_id, name, args_buffer)
    input_tokens: u32,
}

fn map_responses_sse(
    state: &mut ResponsesStreamState,
    event: &crate::sse::SseEvent,
) -> Vec<StreamEvent> {
    if event.data == "[DONE]" {
        return vec![];
    }

    let data: serde_json::Value = match serde_json::from_str(&event.data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let event_type = data["type"].as_str().unwrap_or("");

    match event_type {
        // Text content deltas
        "response.output_text.delta" => {
            let delta = data["delta"].as_str().unwrap_or("");
            if delta.is_empty() {
                vec![]
            } else {
                vec![StreamEvent::TextDelta(delta.to_string())]
            }
        }

        // Reasoning deltas
        "response.reasoning.delta" => {
            let delta = data["delta"].as_str().unwrap_or("");
            if delta.is_empty() {
                vec![]
            } else {
                vec![StreamEvent::ReasoningDelta(delta.to_string())]
            }
        }

        // Function call start
        "response.function_call_arguments.start" => {
            let call_id = data["call_id"]
                .as_str()
                .or_else(|| data["id"].as_str())
                .unwrap_or("")
                .to_string();
            let name = data["name"].as_str().unwrap_or("").to_string();
            let idx = state.tool_calls.len();
            state
                .tool_calls
                .push((call_id.clone(), name.clone(), String::new()));
            vec![StreamEvent::ToolCallDelta {
                index: idx,
                id: Some(call_id),
                name: Some(name),
                arguments_delta: String::new(),
            }]
        }

        // Function call argument deltas
        "response.function_call_arguments.delta" => {
            let delta = data["delta"].as_str().unwrap_or("").to_string();
            if let Some(last) = state.tool_calls.last_mut() {
                last.2.push_str(&delta);
            }
            let idx = state.tool_calls.len().saturating_sub(1);
            vec![StreamEvent::ToolCallDelta {
                index: idx,
                id: None,
                name: None,
                arguments_delta: delta,
            }]
        }

        // Response completed — emit usage + done
        "response.completed" => {
            let usage = &data["response"]["usage"];
            let input = usage["input_tokens"].as_u64().unwrap_or(0) as u32;
            let output = usage["output_tokens"].as_u64().unwrap_or(0) as u32;
            let reasoning = usage["output_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .unwrap_or(0) as u32;

            let has_tool_calls = !state.tool_calls.is_empty();
            let status = data["response"]["status"].as_str().unwrap_or("completed");
            let stop_reason = if has_tool_calls {
                StopReason::ToolUse
            } else {
                match status {
                    "completed" => StopReason::EndTurn,
                    "incomplete" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                }
            };

            vec![
                StreamEvent::Usage(TokenUsage {
                    input_tokens: input,
                    output_tokens: output,
                    reasoning_tokens: reasoning,
                    ..Default::default()
                }),
                StreamEvent::Done(stop_reason),
            ]
        }

        // Capture input token count from response.created
        "response.created" => {
            if let Some(t) = data["response"]["usage"]["input_tokens"].as_u64() {
                state.input_tokens = t as u32;
            }
            vec![]
        }

        _ => vec![],
    }
}

/// Known model prefixes that support the OpenAI Responses API.
/// Exact prefixes avoid false positives on future models (e.g. `gpt-4o-realtime`).
const RESPONSES_PREFIXES: &[&str] = &[
    "o1",
    "o3",
    "o4",
    "gpt-4.1",
    "gpt-5",
    "gpt-4o-mini",
    "gpt-4o-2", // dated snapshots
    "codex",
];

/// Exact model names that support the Responses API.
const RESPONSES_EXACT: &[&str] = &["gpt-4o"];

/// Returns true if a model name is known to benefit from the Responses API.
pub fn is_responses_capable(model: &str) -> bool {
    let m = model.to_lowercase();
    RESPONSES_EXACT.iter().any(|&e| m == e) || RESPONSES_PREFIXES.iter().any(|&p| m.starts_with(p))
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

    #[test]
    fn test_build_input_system_message() {
        let m = msg(MessageRole::System, "be helpful");
        let mut items = Vec::new();
        build_input_items(&m, &mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"].as_str(), Some("system"));
        assert_eq!(items[0]["content"].as_str(), Some("be helpful"));
    }

    #[test]
    fn test_build_input_user_message() {
        let m = msg(MessageRole::User, "hello");
        let mut items = Vec::new();
        build_input_items(&m, &mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"].as_str(), Some("user"));
        assert_eq!(items[0]["content"][0]["type"].as_str(), Some("input_text"));
        assert_eq!(items[0]["content"][0]["text"].as_str(), Some("hello"));
    }

    #[test]
    fn test_build_input_tool_result() {
        let m = Message {
            role: MessageRole::Tool,
            content: "file contents".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_123".into()),
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };
        let mut items = Vec::new();
        build_input_items(&m, &mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"].as_str(), Some("function_call_output"));
        assert_eq!(items[0]["call_id"].as_str(), Some("call_123"));
        assert_eq!(items[0]["output"].as_str(), Some("file contents"));
    }

    #[test]
    fn test_build_input_assistant_with_tool_calls() {
        let m = Message {
            role: MessageRole::Assistant,
            content: "Let me check".into(),
            media: vec![],
            tool_calls: Some(vec![octos_core::ToolCall {
                id: "call_1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "ls"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };
        // Should produce two top-level items: assistant message + function_call
        let mut items = Vec::new();
        build_input_items(&m, &mut items);
        assert_eq!(items.len(), 2);
        // First: assistant message with text only
        assert_eq!(items[0]["role"].as_str(), Some("assistant"));
        let content = items[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"].as_str(), Some("output_text"));
        assert_eq!(content[0]["text"].as_str(), Some("Let me check"));
        // Second: top-level function_call item
        assert_eq!(items[1]["type"].as_str(), Some("function_call"));
        assert_eq!(items[1]["name"].as_str(), Some("shell"));
        assert_eq!(items[1]["call_id"].as_str(), Some("call_1"));
    }

    #[test]
    fn test_build_request_basic() {
        let provider = OpenAIResponsesProvider::new("test-key", "o4-mini");
        let messages = vec![
            msg(MessageRole::System, "system prompt"),
            msg(MessageRole::User, "hello"),
        ];
        let config = ChatConfig::default();
        let request = provider.build_request(&messages, &[], &config);

        assert_eq!(request["model"].as_str(), Some("o4-mini"));
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"].as_str(), Some("system"));
        assert_eq!(input[1]["role"].as_str(), Some("user"));
    }

    #[test]
    fn test_build_request_with_tools() {
        let provider = OpenAIResponsesProvider::new("test-key", "gpt-4.1");
        let messages = vec![msg(MessageRole::User, "hi")];
        let tools = vec![ToolSpec {
            name: "shell".into(),
            description: "Run a command".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let config = ChatConfig::default();
        let request = provider.build_request(&messages, &tools, &config);

        let api_tools = request["tools"].as_array().unwrap();
        assert_eq!(api_tools.len(), 1);
        assert_eq!(api_tools[0]["type"].as_str(), Some("function"));
        assert_eq!(api_tools[0]["name"].as_str(), Some("shell"));
    }

    #[test]
    fn test_build_request_reasoning_effort() {
        let provider = OpenAIResponsesProvider::new("test-key", "o3");
        let messages = vec![msg(MessageRole::User, "think")];
        let mut config = ChatConfig::default();
        config.reasoning_effort = Some(crate::config::ReasoningEffort::High);
        let request = provider.build_request(&messages, &[], &config);

        assert_eq!(request["reasoning"]["effort"].as_str(), Some("high"));
    }

    #[test]
    fn test_parse_response_text_only() {
        let resp = ResponsesApiResponse {
            output: vec![OutputItem::Message {
                content: vec![ContentPart::OutputText {
                    text: "Hello!".into(),
                }],
            }],
            status: "completed".into(),
            usage: ResponsesUsage {
                input_tokens: 10,
                output_tokens: 5,
                output_tokens_details: None,
            },
        };
        let result = parse_responses_api(resp);
        assert_eq!(result.content.as_deref(), Some("Hello!"));
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(result.usage.input_tokens, 10);
    }

    #[test]
    fn test_parse_response_with_function_call() {
        let resp = ResponsesApiResponse {
            output: vec![OutputItem::FunctionCall {
                id: "fc_1".into(),
                call_id: "call_1".into(),
                name: "shell".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            }],
            status: "completed".into(),
            usage: ResponsesUsage::default(),
        };
        let result = parse_responses_api(resp);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "shell");
        assert_eq!(result.tool_calls[0].id, "call_1");
        assert_eq!(result.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn test_parse_response_with_reasoning() {
        let resp = ResponsesApiResponse {
            output: vec![
                OutputItem::Reasoning {
                    content: vec![ReasoningPart {
                        text: "Let me think...".into(),
                    }],
                },
                OutputItem::Message {
                    content: vec![ContentPart::OutputText {
                        text: "The answer is 42.".into(),
                    }],
                },
            ],
            status: "completed".into(),
            usage: ResponsesUsage {
                input_tokens: 20,
                output_tokens: 30,
                output_tokens_details: Some(OutputTokensDetails {
                    reasoning_tokens: 15,
                }),
            },
        };
        let result = parse_responses_api(resp);
        assert_eq!(result.content.as_deref(), Some("The answer is 42."));
        assert_eq!(result.reasoning_content.as_deref(), Some("Let me think..."));
        assert_eq!(result.usage.reasoning_tokens, 15);
    }

    #[test]
    fn test_is_responses_capable() {
        assert!(is_responses_capable("o4-mini"));
        assert!(is_responses_capable("o3-mini"));
        assert!(is_responses_capable("gpt-4.1"));
        assert!(is_responses_capable("gpt-4o"));
        assert!(is_responses_capable("gpt-5"));
        assert!(!is_responses_capable("deepseek-chat"));
        assert!(!is_responses_capable("claude-3"));
    }

    #[test]
    fn test_sse_text_delta() {
        let mut state = ResponsesStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"type": "response.output_text.delta", "delta": "Hello"}"#.into(),
        };
        let events = map_responses_sse(&mut state, &event);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "Hello"));
    }

    #[test]
    fn test_sse_function_call_flow() {
        let mut state = ResponsesStreamState::default();

        // Start
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"type": "response.function_call_arguments.start", "call_id": "c1", "name": "shell"}"#.into(),
        };
        let events = map_responses_sse(&mut state, &event);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ToolCallDelta {
                index, id, name, ..
            } => {
                assert_eq!(*index, 0);
                assert_eq!(id.as_deref(), Some("c1"));
                assert_eq!(name.as_deref(), Some("shell"));
            }
            _ => panic!("expected ToolCallDelta"),
        }

        // Delta
        let event = crate::sse::SseEvent {
            event: None,
            data:
                r#"{"type": "response.function_call_arguments.delta", "delta": "{\"cmd\":\"ls\"}"}"#
                    .into(),
        };
        let events = map_responses_sse(&mut state, &event);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], StreamEvent::ToolCallDelta { arguments_delta, .. } if arguments_delta.contains("cmd"))
        );
    }

    #[test]
    fn test_sse_completed() {
        let mut state = ResponsesStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 100, "output_tokens": 50, "output_tokens_details": {"reasoning_tokens": 20}}}}"#.into(),
        };
        let events = map_responses_sse(&mut state, &event);
        assert_eq!(events.len(), 2);
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 100);
                assert_eq!(u.output_tokens, 50);
                assert_eq!(u.reasoning_tokens, 20);
            }
            _ => panic!("expected Usage"),
        }
        assert!(matches!(&events[1], StreamEvent::Done(StopReason::EndTurn)));
    }

    #[test]
    fn test_sse_done_sentinel() {
        let mut state = ResponsesStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: "[DONE]".into(),
        };
        let events = map_responses_sse(&mut state, &event);
        assert!(events.is_empty());
    }

    #[test]
    fn test_provider_metadata() {
        let provider = OpenAIResponsesProvider::new("key", "o4-mini");
        assert_eq!(provider.model_id(), "o4-mini");
        assert_eq!(provider.provider_name(), "openai");
    }
}
