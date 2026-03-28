//! Google Gemini provider implementation.

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

/// Google Gemini provider.
pub struct GeminiProvider {
    client: Client,
    api_key: SecretString,
    model: String,
    base_url: String,
}

impl GeminiProvider {
    /// Create a new Gemini provider.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::provider::build_http_client(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
                crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            ),
            api_key: SecretString::from(api_key.into()),
            model: model.into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
        }
    }

    /// Create a provider using the GEMINI_API_KEY environment variable.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("GEMINI_API_KEY")
            .or_else(|_| std::env::var("GOOGLE_API_KEY"))
            .wrap_err("GEMINI_API_KEY or GOOGLE_API_KEY environment variable not set")?;
        Ok(Self::new(api_key, "gemini-2.5-flash"))
    }

    /// Set a custom base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Replace the HTTP client with one using custom timeouts (in seconds).
    pub fn with_http_timeout(mut self, timeout_secs: u64, connect_timeout_secs: u64) -> Self {
        self.client = crate::provider::build_http_client(timeout_secs, connect_timeout_secs);
        self
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let (contents, system_instruction) = build_gemini_contents(messages);

        // Build tools array
        let gemini_tools: Option<Vec<GeminiTool>> = if tools.is_empty() {
            None
        } else {
            Some(vec![GeminiTool {
                function_declarations: tools
                    .iter()
                    .map(|t| {
                        let mut params = t.input_schema.clone();
                        sanitize_schema_for_gemini(&mut params);
                        GeminiFunctionDeclaration {
                            name: t.name.clone(),
                            description: t.description.clone(),
                            parameters: params,
                        }
                    })
                    .collect(),
            }])
        };

        let request = GeminiRequest {
            contents,
            system_instruction: system_instruction.map(|text| GeminiSystemInstruction {
                parts: vec![GeminiPart::Text { text }],
            }),
            tools: gemini_tools,
            generation_config: Some(build_gemini_generation_config(config)),
            cached_content: None,
        };

        let url = format!("{}/models/{}:generateContent", self.base_url, self.model);

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("x-goog-api-key", self.api_key.expose_secret())
            .timeout(std::time::Duration::from_secs(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
            ))
            .json(&request)
            .send()
            .await
            .wrap_err("failed to send request to Gemini")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            eyre::bail!(
                "Gemini API error: {status} - {}",
                crate::provider::truncate_error_body(&body)
            );
        }

        let response_text = response
            .text()
            .await
            .wrap_err("failed to read Gemini response body")?;
        let api_response: GeminiResponse =
            serde_json::from_str(&response_text).wrap_err("failed to parse Gemini response")?;

        // Extract content from response
        let candidate = api_response
            .candidates
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("no candidates in Gemini response"))?;

        let mut content = None;
        let mut tool_calls = Vec::new();

        for part in candidate.content.parts {
            match part {
                GeminiPart::Text { text } => {
                    content = Some(text);
                }
                GeminiPart::FunctionCall {
                    function_call,
                    thought_signature,
                } => {
                    let metadata = thought_signature
                        .map(|sig| serde_json::json!({ "thought_signature": sig }));
                    tool_calls.push(octos_core::ToolCall {
                        id: format!("call_{}", tool_calls.len()),
                        name: function_call.name,
                        arguments: function_call.args,
                        metadata,
                    });
                }
                GeminiPart::InlineData { .. } | GeminiPart::FunctionResponse { .. } => {
                    // InlineData and FunctionResponse are only used in requests
                }
            }
        }

        let stop_reason = match candidate.finish_reason.as_deref() {
            Some("STOP") => StopReason::EndTurn,
            Some("MAX_TOKENS") => StopReason::MaxTokens,
            Some("SAFETY" | "RECITATION" | "OTHER" | "BLOCKLIST" | "PROHIBITED_CONTENT") => {
                StopReason::ContentFiltered
            }
            Some("MALFORMED_FUNCTION_CALL") => {
                // Gemini sometimes fails to format tool calls properly.
                // Treat as empty response so the retry logic picks it up.
                tracing::warn!("Gemini returned MALFORMED_FUNCTION_CALL");
                StopReason::EndTurn
            }
            _ if !tool_calls.is_empty() => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        };

        // Gemini doesn't always return usage in the same format
        let usage = api_response.usage_metadata.unwrap_or(GeminiUsageMetadata {
            prompt_token_count: 0,
            candidates_token_count: 0,
            thoughts_token_count: 0,
            cached_content_token_count: 0,
        });

        Ok(ChatResponse {
            content,
            reasoning_content: None,
            tool_calls,
            stop_reason,
            usage: TokenUsage {
                input_tokens: usage.prompt_token_count,
                output_tokens: usage.candidates_token_count,
                reasoning_tokens: usage.thoughts_token_count,
                cache_read_tokens: usage.cached_content_token_count,
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
        let (contents, system_instruction) = build_gemini_contents(messages);

        let gemini_tools: Option<Vec<GeminiTool>> = if tools.is_empty() {
            None
        } else {
            Some(vec![GeminiTool {
                function_declarations: tools
                    .iter()
                    .map(|t| {
                        let mut params = t.input_schema.clone();
                        sanitize_schema_for_gemini(&mut params);
                        GeminiFunctionDeclaration {
                            name: t.name.clone(),
                            description: t.description.clone(),
                            parameters: params,
                        }
                    })
                    .collect(),
            }])
        };

        let request = GeminiRequest {
            contents,
            system_instruction: system_instruction.map(|text| GeminiSystemInstruction {
                parts: vec![GeminiPart::Text { text }],
            }),
            tools: gemini_tools,
            generation_config: Some(build_gemini_generation_config(config)),
            cached_content: None,
        };

        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, self.model
        );

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("x-goog-api-key", self.api_key.expose_secret())
            .json(&request)
            .send()
            .await
            .wrap_err("failed to send streaming request to Gemini")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            eyre::bail!(
                "Gemini API error: {status} - {}",
                crate::provider::truncate_error_body(&text)
            );
        }

        let sse_stream = crate::sse::parse_sse_response(response);
        let state = GeminiStreamState::default();
        let event_stream = sse_stream
            .scan(state, |state, event| {
                let events = map_gemini_sse(state, &event);
                futures::future::ready(Some(events))
            })
            .flat_map(futures::stream::iter);

        Ok(Box::pin(event_stream))
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "gemini"
    }
}

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
    #[serde(rename = "cachedContent", skip_serializing_if = "Option::is_none")]
    cached_content: Option<String>,
}

#[derive(Serialize)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    #[serde(default)]
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum GeminiPart {
    Text {
        text: String,
    },
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: GeminiInlineData,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
        /// Gemini thinking models require this signature to be echoed back.
        /// This is at the part level, NOT inside the functionCall object.
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
}

#[derive(Serialize, Deserialize)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

/// Build the Gemini generation config from ChatConfig.
fn build_gemini_generation_config(config: &ChatConfig) -> GeminiGenerationConfig {
    use crate::config::{ReasoningEffort, ResponseFormat};

    let thinking_config = config.reasoning_effort.map(|effort| {
        let budget = match effort {
            ReasoningEffort::Low => Some(1024),
            ReasoningEffort::Medium => Some(8192),
            ReasoningEffort::High => None, // let model decide
        };
        GeminiThinkingConfig {
            thinking_budget: budget,
        }
    });

    let (response_mime_type, response_schema) = match &config.response_format {
        Some(ResponseFormat::JsonObject) => (Some("application/json".into()), None),
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            let mut s = schema.clone();
            sanitize_schema_for_gemini(&mut s);
            (Some("application/json".into()), Some(s))
        }
        _ => (None, None),
    };

    GeminiGenerationConfig {
        max_output_tokens: config.max_tokens,
        temperature: config.temperature,
        thinking_config,
        response_mime_type,
        response_schema,
    }
}

/// Build the Gemini `contents` array and optional system instruction from messages.
///
/// Gemini requires:
/// - Assistant messages with tool calls → `model` role with `functionCall` parts
/// - Tool result messages → `user` role with `functionResponse` parts
/// - Consecutive same-role messages are merged (Gemini rejects adjacent same-role turns)
fn build_gemini_contents(messages: &[Message]) -> (Vec<GeminiContent>, Option<String>) {
    let mut contents: Vec<GeminiContent> = Vec::new();
    let mut system_instruction: Option<String> = None;

    // Map tool_call_id → function name so tool results can reference the right name.
    let mut call_id_to_name: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for msg in messages {
        match msg.role {
            octos_core::MessageRole::System => match &mut system_instruction {
                Some(existing) => {
                    existing.push_str("\n\n");
                    existing.push_str(&msg.content);
                }
                None => {
                    system_instruction = Some(msg.content.clone());
                }
            },
            octos_core::MessageRole::User => {
                let parts = build_user_parts(msg);
                push_or_merge(&mut contents, "user", parts);
            }
            octos_core::MessageRole::Assistant => {
                let mut parts = Vec::new();
                // Include text content if non-empty.
                if !msg.content.is_empty() {
                    parts.push(GeminiPart::Text {
                        text: msg.content.clone(),
                    });
                }
                // Include functionCall parts for any tool calls the model made.
                if let Some(ref tcs) = msg.tool_calls {
                    for tc in tcs {
                        call_id_to_name.insert(tc.id.clone(), tc.name.clone());
                        // Restore thought_signature from metadata if present.
                        let thought_signature = tc
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("thought_signature"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        parts.push(GeminiPart::FunctionCall {
                            function_call: GeminiFunctionCall {
                                name: tc.name.clone(),
                                args: tc.arguments.clone(),
                            },
                            thought_signature,
                        });
                    }
                }
                // Gemini requires at least one part; add empty text if everything was empty.
                if parts.is_empty() {
                    parts.push(GeminiPart::Text {
                        text: String::new(),
                    });
                }
                push_or_merge(&mut contents, "model", parts);
            }
            octos_core::MessageRole::Tool => {
                // Resolve function name from the matching tool call.
                let name = msg
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| call_id_to_name.get(id))
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());

                let part = GeminiPart::FunctionResponse {
                    function_response: GeminiFunctionResponse {
                        name,
                        response: serde_json::json!({ "content": msg.content }),
                    },
                };
                push_or_merge(&mut contents, "user", vec![part]);
            }
        }
    }
    (contents, system_instruction)
}

/// Merge parts into the last content entry if roles match (Gemini rejects adjacent same-role).
///
/// However, Gemini also silently fails when `functionResponse` parts are mixed with `text`
/// parts in the same turn. To avoid this, we only merge parts of compatible types:
/// functionResponse parts merge with other functionResponse parts, and text/inlineData
/// parts merge with other text/inlineData parts.
fn push_or_merge(contents: &mut Vec<GeminiContent>, role: &str, parts: Vec<GeminiPart>) {
    if let Some(last) = contents.last_mut() {
        if last.role == role && parts_compatible(&last.parts, &parts) {
            last.parts.extend(parts);
            return;
        }
    }
    contents.push(GeminiContent {
        role: role.to_string(),
        parts,
    });
}

/// Check if two sets of parts can be merged without mixing incompatible types.
fn parts_compatible(existing: &[GeminiPart], new: &[GeminiPart]) -> bool {
    let existing_has_func_response = existing
        .iter()
        .any(|p| matches!(p, GeminiPart::FunctionResponse { .. }));
    let new_has_func_response = new
        .iter()
        .any(|p| matches!(p, GeminiPart::FunctionResponse { .. }));
    let existing_has_text = existing
        .iter()
        .any(|p| matches!(p, GeminiPart::Text { .. } | GeminiPart::InlineData { .. }));
    let new_has_text = new
        .iter()
        .any(|p| matches!(p, GeminiPart::Text { .. } | GeminiPart::InlineData { .. }));

    // Don't merge if one side has functionResponse and the other has text
    !((existing_has_func_response && new_has_text) || (existing_has_text && new_has_func_response))
}

fn build_user_parts(msg: &Message) -> Vec<GeminiPart> {
    let images: Vec<_> = msg.media.iter().filter(|p| vision::is_image(p)).collect();

    if images.is_empty() {
        return vec![GeminiPart::Text {
            text: msg.content.clone(),
        }];
    }

    let mut parts = Vec::new();
    for path in images {
        if let Ok((mime, data)) = vision::encode_image(path) {
            parts.push(GeminiPart::InlineData {
                inline_data: GeminiInlineData {
                    mime_type: mime,
                    data,
                },
            });
        }
    }
    if !msg.content.is_empty() {
        parts.push(GeminiPart::Text {
            text: msg.content.clone(),
        });
    }
    parts
}

#[derive(Serialize, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Serialize)]
struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// Maximum recursion depth for schema sanitization (matches MCP limit).
const MAX_SCHEMA_DEPTH: usize = 64;

/// Sanitize a JSON Schema for Gemini's restricted schema support.
///
/// Gemini only supports a subset of JSON Schema. This recursively removes
/// unsupported fields that cause 400 errors or silent empty responses:
/// - `additionalProperties`
/// - Empty `items` schemas (`"items": {}`)
/// - `$schema`, `$ref`, `$id`
fn sanitize_schema_for_gemini(value: &mut serde_json::Value) {
    sanitize_schema_recursive(value, 0);
}

fn sanitize_schema_recursive(value: &mut serde_json::Value, depth: usize) {
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }

    if let Some(obj) = value.as_object_mut() {
        obj.remove("additionalProperties");
        obj.remove("$schema");
        obj.remove("$ref");
        obj.remove("$id");

        // Gemini requires `items` to have a type when present.
        // Replace empty `"items": {}` with `"items": {"type": "string"}`.
        if let Some(items) = obj.get("items") {
            if items.as_object().is_some_and(|o| o.is_empty()) {
                obj.insert("items".to_string(), serde_json::json!({"type": "string"}));
            }
        }

        // Recurse into nested objects
        let keys: Vec<String> = obj.keys().cloned().collect();
        for key in keys {
            if let Some(v) = obj.get_mut(&key) {
                sanitize_schema_recursive(v, depth + 1);
            }
        }
    } else if let Some(arr) = value.as_array_mut() {
        for item in arr {
            sanitize_schema_recursive(item, depth + 1);
        }
    }
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
    #[serde(rename = "responseMimeType", skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<String>,
    #[serde(rename = "responseSchema", skip_serializing_if = "Option::is_none")]
    response_schema: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct GeminiThinkingConfig {
    #[serde(rename = "thinkingBudget", skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<u32>,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiContent,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct GeminiUsageMetadata {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: u32,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates_token_count: u32,
    #[serde(rename = "thoughtsTokenCount", default)]
    thoughts_token_count: u32,
    #[serde(rename = "cachedContentTokenCount", default)]
    cached_content_token_count: u32,
}

// --- Streaming SSE helpers ---

#[derive(Default)]
struct GeminiStreamState {
    tool_count: usize,
    has_tool_calls: bool,
}

// Visible for testing
fn map_gemini_sse(state: &mut GeminiStreamState, event: &crate::sse::SseEvent) -> Vec<StreamEvent> {
    let data: serde_json::Value = match serde_json::from_str(&event.data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut events = Vec::new();

    if let Some(candidates) = data["candidates"].as_array() {
        if let Some(candidate) = candidates.first() {
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                for part in parts {
                    if let Some(text) = part["text"].as_str() {
                        if !text.is_empty() {
                            events.push(StreamEvent::TextDelta(text.to_string()));
                        }
                    }
                    if let Some(fc) = part.get("functionCall") {
                        state.has_tool_calls = true;
                        let name = fc["name"].as_str().unwrap_or("").to_string();
                        let args = fc
                            .get("args")
                            .cloned()
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                        // Capture thought_signature for Gemini thinking models.
                        // thoughtSignature is at the part level, not inside functionCall.
                        let thought_sig = part
                            .get("thoughtSignature")
                            .and_then(|v| v.as_str())
                            .map(|s| serde_json::json!({ "thought_signature": s }));
                        events.push(StreamEvent::ToolCallDelta {
                            index: state.tool_count,
                            id: Some(format!("call_{}", state.tool_count)),
                            name: Some(name),
                            arguments_delta: args.to_string(),
                        });
                        // Emit metadata as a separate event so the agent can store it.
                        if let Some(meta) = thought_sig {
                            events.push(StreamEvent::ToolCallMetadata {
                                index: state.tool_count,
                                metadata: meta,
                            });
                        }
                        state.tool_count += 1;
                    }
                }
            }

            if let Some(reason) = candidate["finishReason"].as_str() {
                let stop_reason = match reason {
                    "STOP" if state.has_tool_calls => StopReason::ToolUse,
                    "STOP" => StopReason::EndTurn,
                    "MAX_TOKENS" => StopReason::MaxTokens,
                    "SAFETY" | "RECITATION" | "OTHER" | "BLOCKLIST" | "PROHIBITED_CONTENT" => {
                        StopReason::ContentFiltered
                    }
                    "MALFORMED_FUNCTION_CALL" => {
                        tracing::warn!("Gemini returned MALFORMED_FUNCTION_CALL (streaming)");
                        StopReason::EndTurn
                    }
                    _ if state.has_tool_calls => StopReason::ToolUse,
                    _ => StopReason::EndTurn,
                };
                events.push(StreamEvent::Done(stop_reason));
            }
        }
    }

    if let Some(usage) = data.get("usageMetadata").filter(|u| !u.is_null()) {
        let input = usage["promptTokenCount"].as_u64().unwrap_or(0) as u32;
        let output = usage["candidatesTokenCount"].as_u64().unwrap_or(0) as u32;
        let thinking = usage["thoughtsTokenCount"].as_u64().unwrap_or(0) as u32;
        let cached = usage["cachedContentTokenCount"].as_u64().unwrap_or(0) as u32;
        if input > 0 || output > 0 {
            events.push(StreamEvent::Usage(TokenUsage {
                input_tokens: input,
                output_tokens: output,
                reasoning_tokens: thinking,
                cache_read_tokens: cached,
                ..Default::default()
            }));
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::{Message, MessageRole, ToolCall};

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

    // --- sanitize_schema_for_gemini tests ---

    #[test]
    fn test_sanitize_removes_additional_properties() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "additionalProperties": false
        });
        sanitize_schema_for_gemini(&mut schema);
        assert!(schema.get("additionalProperties").is_none());
    }

    #[test]
    fn test_sanitize_removes_dollar_fields() {
        let mut schema = serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$ref": "#/definitions/Foo",
            "$id": "my-schema",
            "type": "object"
        });
        sanitize_schema_for_gemini(&mut schema);
        assert!(schema.get("$schema").is_none());
        assert!(schema.get("$ref").is_none());
        assert!(schema.get("$id").is_none());
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn test_sanitize_replaces_empty_items() {
        let mut schema = serde_json::json!({
            "type": "array",
            "items": {}
        });
        sanitize_schema_for_gemini(&mut schema);
        assert_eq!(schema["items"]["type"], "string");
    }

    #[test]
    fn test_sanitize_preserves_non_empty_items() {
        let mut schema = serde_json::json!({
            "type": "array",
            "items": {"type": "integer"}
        });
        sanitize_schema_for_gemini(&mut schema);
        assert_eq!(schema["items"]["type"], "integer");
    }

    #[test]
    fn test_sanitize_recursive() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "nested": {
                    "type": "object",
                    "additionalProperties": true,
                    "properties": {
                        "list": {
                            "type": "array",
                            "items": {}
                        }
                    }
                }
            }
        });
        sanitize_schema_for_gemini(&mut schema);
        assert!(
            schema["properties"]["nested"]
                .get("additionalProperties")
                .is_none()
        );
        assert_eq!(
            schema["properties"]["nested"]["properties"]["list"]["items"]["type"],
            "string"
        );
    }

    // --- build_gemini_contents tests ---

    #[test]
    fn test_build_contents_system_extracted() {
        let messages = vec![
            msg(MessageRole::System, "You are helpful"),
            msg(MessageRole::User, "Hi"),
        ];
        let (contents, system) = build_gemini_contents(&messages);
        assert_eq!(system.as_deref(), Some("You are helpful"));
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
    }

    #[test]
    fn test_build_contents_assistant_mapped_to_model() {
        let messages = vec![
            msg(MessageRole::User, "Hi"),
            msg(MessageRole::Assistant, "Hello!"),
        ];
        let (contents, _) = build_gemini_contents(&messages);
        assert_eq!(contents[1].role, "model");
    }

    #[test]
    fn test_build_contents_tool_call_and_result() {
        let messages = vec![
            msg(MessageRole::User, "read file"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "tc1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "foo.rs"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "file contents".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("tc1".into()),
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            },
        ];
        let (contents, _) = build_gemini_contents(&messages);
        // user, model (with functionCall), user (with functionResponse)
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[1].role, "model");
        assert_eq!(contents[2].role, "user");
    }

    #[test]
    fn test_build_contents_merges_consecutive_same_role() {
        let messages = vec![
            msg(MessageRole::User, "first"),
            msg(MessageRole::User, "second"),
        ];
        let (contents, _) = build_gemini_contents(&messages);
        // Should merge into 1 user turn
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].parts.len(), 2);
    }

    #[test]
    fn test_parts_compatible_blocks_mixed_types() {
        let text = vec![GeminiPart::Text { text: "hi".into() }];
        let func_resp = vec![GeminiPart::FunctionResponse {
            function_response: GeminiFunctionResponse {
                name: "test".into(),
                response: serde_json::json!({"content": "ok"}),
            },
        }];
        assert!(!parts_compatible(&text, &func_resp));
        assert!(!parts_compatible(&func_resp, &text));
        assert!(parts_compatible(&text, &text));
        assert!(parts_compatible(&func_resp, &func_resp));
    }

    // --- SSE mapping tests ---

    #[test]
    fn test_gemini_sse_text_delta() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"candidates": [{"content": {"parts": [{"text": "Hello"}]}}]}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "Hello"));
    }

    #[test]
    fn test_gemini_sse_function_call() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"candidates": [{"content": {"parts": [{"functionCall": {"name": "shell", "args": {"command": "ls"}}}]}}]}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert!(events.iter().any(|e| matches!(e, StreamEvent::ToolCallDelta { name, .. } if name.as_deref() == Some("shell"))));
        assert!(state.has_tool_calls);
    }

    #[test]
    fn test_gemini_sse_finish_reason() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"candidates": [{"content": {"parts": [{"text": "done"}]}, "finishReason": "STOP"}]}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Done(StopReason::EndTurn)))
        );
    }

    #[test]
    fn test_gemini_sse_finish_with_tools() {
        let mut state = GeminiStreamState::default();
        state.has_tool_calls = true;
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"candidates": [{"content": {"parts": []}, "finishReason": "STOP"}]}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Done(StopReason::ToolUse)))
        );
    }

    #[test]
    fn test_gemini_sse_usage() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 50}}"#
                .into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::Usage(u) if u.input_tokens == 100 && u.output_tokens == 50)
        ));
    }

    #[test]
    fn test_gemini_sse_invalid_json() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: "not valid json".into(),
        };
        assert!(map_gemini_sse(&mut state, &event).is_empty());
    }

    // --- Provider metadata tests ---

    #[test]
    fn test_provider_name_and_model() {
        let provider = GeminiProvider::new("test-key", "gemini-2.5-flash");
        assert_eq!(provider.provider_name(), "gemini");
        assert_eq!(provider.model_id(), "gemini-2.5-flash");
    }

    #[test]
    fn test_with_base_url() {
        let provider =
            GeminiProvider::new("key", "model").with_base_url("https://custom.googleapis.com");
        assert_eq!(provider.base_url, "https://custom.googleapis.com");
    }

    // --- Generation config tests ---

    #[test]
    fn test_thinking_config_low_effort() {
        use crate::config::ReasoningEffort;
        let config = ChatConfig {
            reasoning_effort: Some(ReasoningEffort::Low),
            ..Default::default()
        };
        let gen_config = build_gemini_generation_config(&config);
        let tc = gen_config.thinking_config.unwrap();
        assert_eq!(tc.thinking_budget, Some(1024));
    }

    #[test]
    fn test_thinking_config_high_effort() {
        use crate::config::ReasoningEffort;
        let config = ChatConfig {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let gen_config = build_gemini_generation_config(&config);
        let tc = gen_config.thinking_config.unwrap();
        assert!(tc.thinking_budget.is_none());
    }

    #[test]
    fn test_no_thinking_config_by_default() {
        let config = ChatConfig::default();
        let gen_config = build_gemini_generation_config(&config);
        assert!(gen_config.thinking_config.is_none());
    }

    #[test]
    fn test_response_format_json_object() {
        use crate::config::ResponseFormat;
        let config = ChatConfig {
            response_format: Some(ResponseFormat::JsonObject),
            ..Default::default()
        };
        let gen_config = build_gemini_generation_config(&config);
        assert_eq!(
            gen_config.response_mime_type.as_deref(),
            Some("application/json")
        );
        assert!(gen_config.response_schema.is_none());
    }

    #[test]
    fn test_response_format_json_schema() {
        use crate::config::ResponseFormat;
        let config = ChatConfig {
            response_format: Some(ResponseFormat::JsonSchema {
                name: "test".into(),
                schema: serde_json::json!({"type": "object", "additionalProperties": false}),
                strict: true,
            }),
            ..Default::default()
        };
        let gen_config = build_gemini_generation_config(&config);
        assert_eq!(
            gen_config.response_mime_type.as_deref(),
            Some("application/json")
        );
        // additionalProperties should be sanitized away
        let schema = gen_config.response_schema.unwrap();
        assert!(schema.get("additionalProperties").is_none());
    }

    #[test]
    fn test_gemini_sse_usage_with_thinking_tokens() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 50, "thoughtsTokenCount": 20, "cachedContentTokenCount": 30}}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::Usage(u) if u.reasoning_tokens == 20 && u.cache_read_tokens == 30)
        ));
    }
}
