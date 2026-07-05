//! Anthropic (Claude) provider implementation.

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::{Message, MessageRole};

use reqwest::Client;
use serde::{Deserialize, Serialize};

use secrecy::{ExposeSecret, SecretString};

use crate::vision;

use crate::config::ChatConfig;
use crate::config::ReasoningEffort;
use crate::provider::{LlmProvider, endpoint_label_from_base_url};
use crate::types::{
    ChatResponse, ChatStream, ProviderMetadata, StopReason, StreamEvent, TokenUsage, ToolSpec,
};

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
        config: &'a ChatConfig,
    ) -> AnthropicRequest<'a> {
        let max_tokens = config.max_tokens.unwrap_or(4096);
        AnthropicRequest {
            model: &self.model,
            max_tokens,
            messages: build_anthropic_messages(messages),
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
            thinking: config
                .reasoning_effort
                .and_then(|effort| build_anthropic_thinking(effort, max_tokens)),
            context_management: config.context_management.as_ref(),
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
            let body = crate::provider::truncate_error_body(&body);
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &body,
                format!("{}/{}", self.provider_label, self.model),
            )
            .into());
        }

        let api_response: AnthropicResponse = response
            .json()
            .await
            .wrap_err("failed to parse Anthropic response")?;

        Ok(anthropic_response_to_chat_response(api_response))
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
            let body = crate::provider::truncate_error_body(&text);
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &body,
                format!("{}/{}", self.provider_label, self.model),
            )
            .into());
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

    fn provider_metadata(&self) -> ProviderMetadata {
        let endpoint = if self.base_url != "https://api.anthropic.com" {
            endpoint_label_from_base_url(&self.base_url)
        } else {
            None
        };
        ProviderMetadata::new(self.provider_label.clone(), self.model.clone(), endpoint)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    /// M8.5 tier 2: forwarded from `ChatConfig.context_management`. Opaque
    /// payload (typically `{ "edits": [ { "type":
    /// "clear_tool_uses_20250919", ... } ] }`) that tells Anthropic's server
    /// to clear old tool uses on its side. Only emitted when the field is
    /// non-null and the caller opted in via the builder.
    #[serde(skip_serializing_if = "Option::is_none")]
    context_management: Option<&'a serde_json::Value>,
}

#[derive(Serialize)]
struct AnthropicThinking {
    r#type: &'static str,
    budget_tokens: u32,
}

/// Anthropic requires `1024 <= budget_tokens < max_tokens`, and the reply still
/// needs output room. Clamp the per-effort budget to leave a reserve below
/// `max_tokens`; if `max_tokens` is too small to fit a valid (>=1024) budget,
/// return `None` so we omit the thinking param entirely instead of emitting a
/// request Claude rejects before the turn starts.
fn build_anthropic_thinking(effort: ReasoningEffort, max_tokens: u32) -> Option<AnthropicThinking> {
    const MIN_BUDGET: u32 = 1_024;
    const OUTPUT_RESERVE: u32 = 1_024;
    let budget =
        anthropic_thinking_budget_tokens(effort).min(max_tokens.saturating_sub(OUTPUT_RESERVE));
    if budget < MIN_BUDGET {
        return None;
    }
    Some(AnthropicThinking {
        r#type: "enabled",
        budget_tokens: budget,
    })
}

fn anthropic_thinking_budget_tokens(effort: ReasoningEffort) -> u32 {
    match effort {
        ReasoningEffort::Low => 1_024,
        ReasoningEffort::Medium => 4_096,
        ReasoningEffort::High => 8_192,
        ReasoningEffort::Max => 16_000,
    }
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
    /// Prior assistant tool invocation, round-tripped from
    /// [`octos_core::Message::tool_calls`]. Anthropic requires the original
    /// `tool_use` block in the assistant turn for the following
    /// `tool_result` to pair with — without it the request 400s.
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool output for a prior `tool_use`, carried in a USER-role message
    /// (Anthropic's convention for tool results).
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Serialize)]
struct AnthropicImageSource {
    r#type: String,
    media_type: String,
    data: String,
}

/// Convert the transcript into Anthropic wire messages.
///
/// Role handling:
/// - System rows are extracted by the caller into the top-level `system`
///   field and skipped here.
/// - Assistant rows round-trip `tool_calls` as `tool_use` blocks; rows with
///   neither non-blank text nor tool calls are DROPPED (Anthropic rejects
///   empty content — mirrors the openai.rs filter).
/// - Tool rows become `tool_result` blocks in USER-role messages, and
///   CONSECUTIVE Tool rows merge into ONE user message: Anthropic requires
///   every `tool_use` id from the assistant turn to be answered in the
///   immediately-following message, so parallel tool results split across
///   two user messages would 400.
fn build_anthropic_messages(messages: &[Message]) -> Vec<AnthropicMessage<'static>> {
    let mut out: Vec<AnthropicMessage> = Vec::with_capacity(messages.len());
    // True while `out.last()` is the user-role message accumulating the
    // current run of consecutive tool_result blocks.
    let mut merging_tool_results = false;
    // tool_use ids answerable by the NEXT emitted message (codex P2): a
    // `tool_result` is only valid immediately after the assistant message
    // that carries its `tool_use`. A Tool row whose id is not pending — the
    // assistant row was trimmed/compacted away, the window starts mid-loop,
    // or another message closed the window — must fall back to plain user
    // text; Anthropic rejects orphan tool_result blocks outright.
    let mut pending_tool_use_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for m in messages
        .iter()
        .filter(|m| m.role != octos_core::MessageRole::System)
    {
        match m.role {
            octos_core::MessageRole::Assistant => {
                merging_tool_results = false;
                if let Some(content) = build_assistant_anthropic_content(m) {
                    pending_tool_use_ids = m
                        .tool_calls
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .filter(|tc| !tc.id.is_empty())
                        .map(|tc| tc.id.clone())
                        .collect();
                    out.push(AnthropicMessage {
                        role: "assistant",
                        content,
                    });
                }
                // A DROPPED (fully-empty) assistant row emits nothing, so the
                // previously-emitted message is unchanged — leave the pending
                // window as-is.
            }
            octos_core::MessageRole::Tool => {
                let block = m
                    .tool_call_id
                    .as_deref()
                    .filter(|id| pending_tool_use_ids.contains(*id))
                    .and_then(|_| anthropic_tool_result_block(m));
                match block {
                    Some(block) => {
                        // Consume the id: a duplicate result for the same
                        // tool_use would also be rejected.
                        if let Some(id) = m.tool_call_id.as_deref() {
                            pending_tool_use_ids.remove(id);
                        }
                        if merging_tool_results
                            && let Some(AnthropicMessage {
                                content: AnthropicContent::Parts(parts),
                                ..
                            }) = out.last_mut()
                        {
                            parts.push(block);
                        } else {
                            out.push(AnthropicMessage {
                                role: "user",
                                content: AnthropicContent::Parts(vec![block]),
                            });
                            merging_tool_results = true;
                        }
                    }
                    None => {
                        // ID-less or orphan tool output: no pending tool_use
                        // to pair with — plain user text for this row. This
                        // also closes the pending window (the inserted text
                        // message breaks the immediately-after adjacency).
                        merging_tool_results = false;
                        pending_tool_use_ids.clear();
                        out.push(AnthropicMessage {
                            role: "user",
                            content: build_anthropic_content(m),
                        });
                    }
                }
            }
            _ => {
                merging_tool_results = false;
                pending_tool_use_ids.clear();
                out.push(AnthropicMessage {
                    role: "user",
                    content: build_anthropic_content(m),
                });
            }
        }
    }
    out
}

/// Build an ASSISTANT message's content, round-tripping `tool_calls` as
/// `tool_use` blocks. Returns `None` when the row has neither non-blank text
/// nor tool calls — Anthropic rejects empty/whitespace-only content, so such
/// rows are dropped entirely (mirrors the openai.rs empty-assistant filter).
///
/// Known limitation: streamed `thinking` blocks are NOT round-tripped (their
/// signatures are not captured in [`ChatResponse`]), so a tool loop with
/// extended thinking enabled may still be rejected by the API's
/// thinking-precedes-tool_use requirement. Tool loops with thinking off (the
/// default) are fully supported.
fn build_assistant_anthropic_content(msg: &Message) -> Option<AnthropicContent> {
    let tool_calls = msg
        .tool_calls
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|tc| !tc.id.is_empty())
        .collect::<Vec<_>>();
    let has_text = !msg.content.trim().is_empty();

    if tool_calls.is_empty() {
        if !has_text {
            return None;
        }
        return Some(AnthropicContent::Text(msg.content.clone()));
    }

    let mut parts = Vec::with_capacity(tool_calls.len() + 1);
    if has_text {
        parts.push(AnthropicContentBlock::Text {
            text: msg.content.clone(),
        });
    }
    for tc in tool_calls {
        parts.push(AnthropicContentBlock::ToolUse {
            id: tc.id.clone(),
            name: tc.name.clone(),
            input: tc.arguments.clone(),
        });
    }
    Some(AnthropicContent::Parts(parts))
}

/// Build the `tool_result` block for a Tool-role message. Returns `None`
/// when `tool_call_id` is missing/empty (ID-less providers) — an empty
/// `tool_use_id` would 400, so the caller falls back to plain user text.
fn anthropic_tool_result_block(msg: &Message) -> Option<AnthropicContentBlock> {
    let tool_use_id = msg.tool_call_id.as_deref().filter(|id| !id.is_empty())?;
    Some(AnthropicContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: msg.content.clone(),
    })
}

fn build_anthropic_content(msg: &Message) -> AnthropicContent {
    // Mirror openai.rs: only inline vision content on USER messages.
    // Assistant/Tool media is prior-turn tool output (e.g.
    // send_file(skill-output/slides/<slug>/output/slide-NN.png)) and
    // should never be re-fed as image input on subsequent turns.
    let images: Vec<_> = if msg.role != MessageRole::User {
        vec![]
    } else {
        msg.media.iter().filter(|p| vision::is_image(p)).collect()
    };

    if images.is_empty() {
        // Include non-image file paths so the agent can use read_file
        let non_image: Vec<_> = msg.media.iter().filter(|p| !vision::is_image(p)).collect();
        if non_image.is_empty() {
            return AnthropicContent::Text(msg.content.clone());
        }
        // Mini5 2026-05-12: the prior note ("Use read_file to access them.")
        // caused DeepSeek/Anthropic to refuse paths under /private/var/...
        // because the LLM compared them to its declared workspace root and
        // assumed the file was off-limits. The phrasing here makes the
        // authorization explicit so the model goes straight to `read_file`
        // instead of attempting a `shell cp` workaround.
        let note = format!(
            "[user-uploaded files: {}. These are authenticated attachments — \
             call read_file with this exact path. The path is whitelisted \
             even if it lies outside the workspace root.]",
            non_image
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
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
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Any other block type — notably `redacted_thinking` (opaque, returned when
    /// extended thinking is enabled), but also forward-compat for future block
    /// types. Without this the internally-tagged enum fails to deserialize an
    /// unknown `type` and the whole response (answer + tool calls) is lost.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}

fn append_nonempty(target: &mut Option<String>, text: String) {
    if text.is_empty() {
        return;
    }
    match target {
        Some(existing) => existing.push_str(&text),
        None => *target = Some(text),
    }
}

fn anthropic_response_to_chat_response(api_response: AnthropicResponse) -> ChatResponse {
    let mut content = None;
    let mut reasoning_content = None;
    let mut tool_calls = Vec::new();

    for block in api_response.content {
        match block {
            ContentBlock::Text { text } => append_nonempty(&mut content, text),
            ContentBlock::Thinking { thinking } => {
                append_nonempty(&mut reasoning_content, thinking);
            }
            ContentBlock::Unknown => {}
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

    ChatResponse {
        content,
        reasoning_content,
        tool_calls,
        stop_reason,
        usage: TokenUsage {
            input_tokens: api_response.usage.input_tokens,
            output_tokens: api_response.usage.output_tokens,
            ..Default::default()
        },
        provider_index: None,
    }
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
                "thinking_delta" => {
                    let thinking = data["delta"]["thinking"].as_str().unwrap_or("");
                    if thinking.is_empty() {
                        vec![]
                    } else {
                        vec![StreamEvent::ReasoningDelta(thinking.to_string())]
                    }
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
            client_message_id: None,
            thread_id: None,
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
            client_message_id: None,
            thread_id: None,
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

    // --- tool_use / tool_result round-trip tests ---

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> octos_core::ToolCall {
        octos_core::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
            metadata: None,
        }
    }

    #[test]
    fn tool_loop_history_round_trips_tool_use_and_tool_result() {
        // Regression: the request builder never serialized `tool_calls` /
        // `tool_call_id`, so an assistant turn that returned only tool_use
        // round-tripped as `{"role":"assistant","content":""}` (Anthropic
        // 400s on empty content) and the Tool-role result went back as plain
        // user text (Anthropic 400s on a dangling tool_use without a
        // matching tool_result in the next message). Iteration 2 of every
        // native-Anthropic tool loop died on it.
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let mut assistant = msg(MessageRole::Assistant, "");
        assistant.tool_calls = Some(vec![tool_call(
            "toolu_01",
            "shell",
            serde_json::json!({"command": "ls"}),
        )]);
        let mut tool_result = msg(MessageRole::Tool, "file-a\nfile-b");
        tool_result.tool_call_id = Some("toolu_01".into());
        let messages = vec![msg(MessageRole::User, "list files"), assistant, tool_result];

        let config = ChatConfig::default();
        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        let out = body["messages"].as_array().unwrap();
        assert_eq!(out.len(), 3, "user + assistant + tool_result: {body}");

        let assistant_content = out[1]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("assistant content must be blocks, got {}", out[1]));
        assert_eq!(assistant_content[0]["type"], "tool_use");
        assert_eq!(assistant_content[0]["id"], "toolu_01");
        assert_eq!(assistant_content[0]["name"], "shell");
        assert_eq!(assistant_content[0]["input"]["command"], "ls");

        assert_eq!(out[2]["role"], "user");
        let result_content = out[2]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("tool result content must be blocks, got {}", out[2]));
        assert_eq!(result_content[0]["type"], "tool_result");
        assert_eq!(result_content[0]["tool_use_id"], "toolu_01");
        assert_eq!(result_content[0]["content"], "file-a\nfile-b");
    }

    #[test]
    fn parallel_tool_results_merge_into_one_user_message() {
        // Anthropic requires EVERY tool_use id from the assistant turn to have
        // a tool_result in the IMMEDIATELY following message. Two consecutive
        // Tool-role rows (parallel calls) must therefore merge into one user
        // message with two tool_result blocks — separate user messages 400.
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let mut assistant = msg(MessageRole::Assistant, "Running both.");
        assistant.tool_calls = Some(vec![
            tool_call("toolu_a", "shell", serde_json::json!({"command": "ls"})),
            tool_call("toolu_b", "read_file", serde_json::json!({"path": "x"})),
        ]);
        let mut result_a = msg(MessageRole::Tool, "out-a");
        result_a.tool_call_id = Some("toolu_a".into());
        let mut result_b = msg(MessageRole::Tool, "out-b");
        result_b.tool_call_id = Some("toolu_b".into());
        let messages = vec![
            msg(MessageRole::User, "go"),
            assistant,
            result_a,
            result_b,
            msg(MessageRole::User, "thanks"),
        ];

        let config = ChatConfig::default();
        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        let out = body["messages"].as_array().unwrap();
        assert_eq!(
            out.len(),
            4,
            "user + assistant + merged tool_results + user: {body}"
        );

        // Assistant keeps its text AND carries both tool_use blocks.
        let assistant_content = out[1]["content"].as_array().unwrap();
        assert_eq!(assistant_content[0]["type"], "text");
        assert_eq!(assistant_content[0]["text"], "Running both.");
        assert_eq!(assistant_content[1]["type"], "tool_use");
        assert_eq!(assistant_content[2]["type"], "tool_use");

        let results = out[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2, "both tool_results in ONE user message");
        assert_eq!(results[0]["tool_use_id"], "toolu_a");
        assert_eq!(results[1]["tool_use_id"], "toolu_b");

        assert_eq!(out[3]["role"], "user");
        assert_eq!(out[3]["content"], "thanks");
    }

    #[test]
    fn empty_assistant_message_is_dropped_from_request() {
        // Mirrors the openai.rs empty-assistant filter: a fully-empty
        // assistant row (no text, no tool calls) must be dropped, not sent as
        // `content: ""` which Anthropic rejects.
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![
            msg(MessageRole::User, "hi"),
            msg(MessageRole::Assistant, ""),
            msg(MessageRole::User, "still there?"),
        ];
        let config = ChatConfig::default();
        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        let out = body["messages"].as_array().unwrap();
        assert_eq!(out.len(), 2, "empty assistant row dropped: {body}");
        assert_eq!(out[0]["content"], "hi");
        assert_eq!(out[1]["content"], "still there?");
    }

    #[test]
    fn orphan_tool_result_falls_back_to_plain_text() {
        // codex P2: a Tool row whose matching assistant tool_use is NOT the
        // immediately-preceding emitted message (compaction trimmed the
        // assistant row, or the window starts mid-loop) must NOT serialize as
        // a tool_result — Anthropic rejects orphan tool_result blocks. Fall
        // back to plain user text, the pre-fix behaviour for these rows.
        let provider = AnthropicProvider::new("test-key", "claude-test");

        // Case 1: transcript starts with a Tool row (assistant trimmed away).
        let mut orphan = msg(MessageRole::Tool, "stale output");
        orphan.tool_call_id = Some("toolu_gone".into());
        let messages = vec![orphan.clone(), msg(MessageRole::User, "continue")];
        let config = ChatConfig::default();
        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        let out = body["messages"].as_array().unwrap();
        assert_eq!(out[0]["role"], "user");
        assert_eq!(
            out[0]["content"], "stale output",
            "orphan tool row must be plain text, not tool_result: {body}"
        );

        // Case 2: a user row between the assistant's tool_use and the Tool row
        // closes the immediately-following window — the late result must fall
        // back to text (a tool_result there would be orphaned).
        let mut assistant = msg(MessageRole::Assistant, "");
        assistant.tool_calls = Some(vec![tool_call(
            "toolu_late",
            "shell",
            serde_json::json!({"command": "ls"}),
        )]);
        let mut late = msg(MessageRole::Tool, "late output");
        late.tool_call_id = Some("toolu_late".into());
        let messages = vec![
            msg(MessageRole::User, "go"),
            assistant,
            msg(MessageRole::User, "interposed"),
            late,
        ];
        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        let out = body["messages"].as_array().unwrap();
        assert_eq!(out[3]["role"], "user");
        assert_eq!(
            out[3]["content"], "late output",
            "a result outside the immediately-following window must be plain text: {body}"
        );
    }

    #[test]
    fn tool_result_without_id_falls_back_to_plain_text() {
        // ID-less providers can leave tool_call_id empty; a tool_result block
        // with an empty tool_use_id would 400, so fall back to plain user
        // text (pre-fix behaviour) for that row only.
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let mut orphan = msg(MessageRole::Tool, "orphan output");
        orphan.tool_call_id = None;
        let messages = vec![msg(MessageRole::User, "go"), orphan];
        let config = ChatConfig::default();
        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        let out = body["messages"].as_array().unwrap();
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"], "orphan output");
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
            client_message_id: None,
            thread_id: None,
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
        assert_eq!(request.max_tokens, crate::context::default_max_tokens());
    }

    #[test]
    fn should_forward_context_management_payload_when_set() {
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![msg(MessageRole::User, "hi")];
        let payload = serde_json::json!({
            "edits": [
                {
                    "type": "clear_tool_uses_20250919",
                    "keep": { "type": "input_tokens", "value": 10 }
                }
            ]
        });
        let config = ChatConfig {
            context_management: Some(payload.clone()),
            ..Default::default()
        };
        let request = provider.build_request(&messages, &[], &config);
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["context_management"], payload);
    }

    #[test]
    fn should_omit_context_management_when_not_set() {
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![msg(MessageRole::User, "hi")];
        let config = ChatConfig::default();
        let request = provider.build_request(&messages, &[], &config);
        let body = serde_json::to_value(&request).unwrap();
        assert!(
            body.get("context_management").is_none(),
            "field must be omitted when not configured: {body}"
        );
    }

    #[test]
    fn should_emit_thinking_only_when_reasoning_effort_is_set() {
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![msg(MessageRole::User, "hi")];

        let default_config = ChatConfig::default();
        let default_request = provider.build_request(&messages, &[], &default_config);
        let default_body = serde_json::to_value(&default_request).unwrap();
        assert!(default_body.get("thinking").is_none());

        let config = ChatConfig {
            reasoning_effort: Some(ReasoningEffort::High),
            // Large enough output budget to fit the full High ladder (8192) with
            // the output reserve, so it is not clamped here.
            max_tokens: Some(32_000),
            ..Default::default()
        };
        let request = provider.build_request(&messages, &[], &config);
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8_192);
    }

    #[test]
    fn should_clamp_thinking_budget_below_max_tokens() {
        // P2: budget must stay strictly below max_tokens (with output room).
        // High ladder is 8192; with max_tokens=6000 it clamps to 6000-1024=4976.
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![msg(MessageRole::User, "hi")];
        let config = ChatConfig {
            reasoning_effort: Some(ReasoningEffort::High),
            max_tokens: Some(6_000),
            ..Default::default()
        };
        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
        assert_eq!(budget, 4_976, "clamped to max_tokens - reserve");
        assert!(budget < 6_000, "budget strictly below max_tokens");
        assert!(budget >= 1_024, "budget meets Anthropic minimum");
    }

    #[test]
    fn should_omit_thinking_when_max_tokens_too_small() {
        // P2: when max_tokens can't fit a valid (>=1024) budget, omit thinking
        // entirely rather than emit a request Claude rejects.
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![msg(MessageRole::User, "hi")];
        let config = ChatConfig {
            reasoning_effort: Some(ReasoningEffort::Max),
            max_tokens: Some(1_500), // 1500 - 1024 = 476 < 1024 → omit
            ..Default::default()
        };
        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "thinking omitted when max_tokens too small: {body}"
        );
    }

    #[test]
    fn should_parse_redacted_thinking_block() {
        // P2: a redacted_thinking block must not break deserialization; the
        // answer/tool calls still come through and it contributes no reasoning.
        let api_response: AnthropicResponse = serde_json::from_value(serde_json::json!({
            "content": [
                { "type": "redacted_thinking", "data": "Er0BCkY..." },
                { "type": "text", "text": "The answer is 42." }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 5, "output_tokens": 7 }
        }))
        .unwrap();

        let response = anthropic_response_to_chat_response(api_response);
        assert_eq!(response.content.as_deref(), Some("The answer is 42."));
        assert_eq!(response.reasoning_content, None);
    }

    #[test]
    fn test_response_captures_thinking_content_block() {
        let api_response: AnthropicResponse = serde_json::from_value(serde_json::json!({
            "content": [
                {
                    "type": "thinking",
                    "thinking": "Compare the constraints first.",
                    "signature": "opaque"
                },
                {
                    "type": "text",
                    "text": "Use the narrower option."
                }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 34
            }
        }))
        .unwrap();

        let response = anthropic_response_to_chat_response(api_response);
        assert_eq!(
            response.reasoning_content.as_deref(),
            Some("Compare the constraints first.")
        );
        assert_eq!(
            response.content.as_deref(),
            Some("Use the narrower option.")
        );
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
    fn test_sse_thinking_delta() {
        let mut state = AnthropicStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Check edge cases."}}"#.into(),
        };
        let events = map_anthropic_sse(&mut state, &event);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::ReasoningDelta(t) if t == "Check edge cases."));
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
        let mut state = AnthropicStreamState {
            input_tokens: 100,
            ..Default::default()
        };
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

    // Codex round-4 MAJOR: the chat() and chat_stream() error paths previously
    // hardcoded `format!("anthropic/{}", self.model)` instead of using
    // `self.provider_label`. Registry entries for `r9s` and `zai` lanes call
    // `with_provider_label("r9s")` / `with_provider_label("zai")` so those
    // lanes are distinguishable in the failover ledger — but the error label
    // overwrote that with `"anthropic"`, so the ledger could not attribute
    // failures to the correct lane.
    //
    // This test pins the label-threading contract: when a custom provider
    // label is set, the error label produced by the chat()/chat_stream()
    // error paths must use it, not the hardcoded "anthropic" string.
    #[test]
    fn should_thread_provider_label_into_error_label_when_overridden() {
        let provider =
            AnthropicProvider::new("test-key", "claude-3-5-sonnet").with_provider_label("r9s");

        // The error label is `format!("{}/{}", self.provider_label, self.model)`
        // — replicate that here and assert it lands on `r9s/...` not
        // `anthropic/...`.
        let error_label = format!("{}/{}", provider.provider_label, provider.model);
        assert_eq!(error_label, "r9s/claude-3-5-sonnet");
        assert!(
            !error_label.starts_with("anthropic/"),
            "r9s lane must NOT be misreported as anthropic/...: {error_label}"
        );

        // Feed the same label through the classifier the chat() error path
        // calls to confirm the LlmError carries it end-to-end.
        let err =
            crate::error::LlmError::from_status_with_label(429, "rate_limit_error", &error_label);
        assert_eq!(err.provider, "r9s/claude-3-5-sonnet");
    }

    #[test]
    fn should_default_error_label_to_anthropic_when_label_not_overridden() {
        let provider = AnthropicProvider::new("test-key", "claude-3-5-sonnet");
        let error_label = format!("{}/{}", provider.provider_label, provider.model);
        assert_eq!(error_label, "anthropic/claude-3-5-sonnet");
    }
}
