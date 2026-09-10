//! OpenAI Responses API provider (`POST /v1/responses`).
//!
//! Supports reasoning token breakdowns, structured output, and native
//! OpenAI tools. Falls back gracefully — the registry selects this
//! provider only for actual OpenAI endpoints with capable models.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::{Message, MessageRole};

use reqwest::Client;
use serde::Deserialize;

use secrecy::{ExposeSecret, SecretString};

use crate::cache_manifest::{
    PromptCacheInputManifest, prompt_cache_features_enabled, without_cache_markers,
};
use crate::config::ChatConfig;
use crate::provider::{LlmProvider, endpoint_label_from_base_url};
use crate::types::ProviderMetadata;
use crate::types::{ChatResponse, ChatStream, StopReason, StreamEvent, TokenUsage, ToolSpec};

mod continuation;
use continuation::{Continuations, Pending, missing_response};

/// OpenAI provider using the Responses API.
pub struct OpenAIResponsesProvider {
    client: Client,
    /// Separate client for streaming requests, built without a total request
    /// timeout so a healthy long generation is never cut off mid-stream. See
    /// [`crate::provider::build_streaming_http_client`].
    stream_client: Client,
    api_key: SecretString,
    model: String,
    base_url: String,
    /// Only official OpenAI Responses endpoints receive reserved prompt-cache
    /// request fields by default. Compatibility endpoints must opt in.
    prompt_cache_affinity: bool,
    response_continuation: bool,
    continuations: Arc<Continuations>,
}

impl OpenAIResponsesProvider {
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
            base_url: "https://api.openai.com/v1".to_string(),
            prompt_cache_affinity: true,
            response_continuation: false,
            continuations: Arc::default(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        self.prompt_cache_affinity = base_url.trim_end_matches('/') == "https://api.openai.com/v1";
        self.base_url = base_url;
        self
    }

    /// Explicit override for an endpoint known to implement the OpenAI
    /// Responses `prompt_cache_key` contract. Custom endpoints stay opt-out
    /// unless their route configuration deliberately enables it.
    pub fn with_prompt_cache_affinity(mut self, enabled: bool) -> Self {
        self.prompt_cache_affinity = enabled;
        self
    }

    /// Opt in only for a route that supports stored Responses. Anonymous
    /// calls and one-shot compaction requests always send their full input.
    pub fn with_response_continuation(mut self, enabled: bool) -> Self {
        self.response_continuation = enabled;
        self
    }

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
            crate::provider::ApiStyle::OpenAiResponses,
        )
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
        if self.base_url != "https://api.openai.com/v1" {
            if let Some(temperature) = config.temperature {
                body["temperature"] = temperature.into();
            }
            if !config.stop_sequences.is_empty() {
                body["stop"] = serde_json::json!(config.stop_sequences);
            }
            if let Some(params) = &config.sampling_params {
                for (key, value) in params {
                    // Sampler configuration cannot replace conversation state.
                    if matches!(
                        key.as_str(),
                        "top_p"
                            | "top_k"
                            | "min_p"
                            | "repetition_penalty"
                            | "frequency_penalty"
                            | "presence_penalty"
                            | "chat_template_kwargs"
                    ) {
                        body[key] = value.clone();
                    }
                }
            }
            if self.model.to_ascii_lowercase().contains("qwen")
                && matches!(
                    config.reasoning_effort,
                    Some(crate::ReasoningEffort::Disabled)
                )
            {
                body["chat_template_kwargs"]["enable_thinking"] = false.into();
            }
        }

        if self.prompt_cache_affinity
            && prompt_cache_features_enabled()
            && let Some(cache) = config.prompt_cache_context.as_ref()
        {
            body["prompt_cache_key"] = cache.affinity_key.clone().into();
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
        if let Some(tool_choice) = config.tool_choice.openai_responses_wire(!tools.is_empty()) {
            body["tool_choice"] = tool_choice;
        }

        // Reasoning effort maps to the reasoning object
        if let Some(effort) = &config.reasoning_effort {
            let effort_str = match effort {
                crate::config::ReasoningEffort::Disabled => "none",
                crate::config::ReasoningEffort::Low => "low",
                crate::config::ReasoningEffort::Medium => "medium",
                crate::config::ReasoningEffort::High => "high",
                // The Responses API reasoning.effort has no "max"; clamp to high.
                crate::config::ReasoningEffort::Max => "high",
            };
            body["reasoning"] = serde_json::json!({
                "effort": effort_str,
            });
        }

        body
    }

    fn prompt_cache_input_manifest(
        &self,
        request: &serde_json::Value,
        config: &ChatConfig,
    ) -> PromptCacheInputManifest {
        let normalized = without_cache_markers(request.clone());
        let mut stable = Vec::new();
        let mut conversation = Vec::new();
        if let Some(input) = normalized
            .get("input")
            .and_then(serde_json::Value::as_array)
        {
            for (index, item) in input.iter().enumerate() {
                let kind = item
                    .get("role")
                    .or_else(|| item.get("type"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let segment = (format!("input:{index}:{kind}"), item.clone());
                if kind == "system" || kind == "developer" {
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
        let metadata = self.provider_metadata();
        PromptCacheInputManifest::from_normalized_segments(
            metadata.provider,
            metadata.model,
            config
                .prompt_cache_context
                .as_ref()
                .map(|context| context.epoch_id.as_str()),
            stable,
            conversation,
        )
    }

    fn trace_prompt_cache_input(&self, request: &serde_json::Value, config: &ChatConfig) {
        if tracing::enabled!(target: "octos.prompt_cache", tracing::Level::TRACE) {
            self.prompt_cache_input_manifest(request, config).trace();
        }
    }

    async fn send_request(
        &self,
        full: &serde_json::Value,
        config: &ChatConfig,
        streaming: bool,
    ) -> Result<(reqwest::Response, Option<Pending>)> {
        self.trace_prompt_cache_input(full, config);
        let mut body = full.clone();
        let pending = if self.response_continuation
            && prompt_cache_features_enabled()
            && config.cache_retention == crate::CacheRetention::Default
        {
            crate::current_router_context()
                .session_id
                .filter(|id| !id.is_empty())
                .and_then(|session| self.continuations.prepare(session, &mut body))
        } else {
            None
        };
        let client = if streaming {
            &self.stream_client
        } else {
            &self.client
        };
        loop {
            tracing::info!(target: "octos.responses", continuation = body.get("previous_response_id").is_some(),
                full_bytes = full.to_string().len(), sent_bytes = body.to_string().len(), "Responses request");
            let response = client
                .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
                .bearer_auth(self.api_key.expose_secret())
                .json(&body)
                .send()
                .await
                .wrap_err_with(|| {
                    crate::provider::transport_error_message(
                        streaming,
                        self.provider_name(),
                        &self.model,
                        crate::provider::ApiStyle::OpenAiResponses,
                    )
                })?;
            if response.status().is_success() {
                return Ok((response, pending));
            }
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if let Some(id) = body["previous_response_id"].as_str()
                && missing_response(status.as_u16(), &text, id)
            {
                self.continuations.forget_id(id);
                body = full.clone();
                body["store"] = true.into();
                tracing::info!(target: "octos.responses", "Stored response unavailable; retrying full history");
                continue;
            }
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &crate::provider::truncate_error_body(&text),
                format!("{}/{}", self.provider_name(), self.model),
            )
            .with_api_style(crate::provider::ApiStyle::OpenAiResponses)
            .into());
        }
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
        let (response, pending) = self.send_request(&body, config, false).await?;
        let raw: serde_json::Value = response.json().await.wrap_err_with(|| {
            self.operational_message(crate::provider::OperationalStage::ParseResponse)
        })?;
        let api_response: ResponsesApiResponse = serde_json::from_value(raw.clone())
            .wrap_err_with(|| {
                self.operational_message(crate::provider::OperationalStage::ParseResponse)
            })?;
        if matches!(api_response.status.as_str(), "failed" | "cancelled") {
            eyre::bail!("Responses request {}", api_response.status);
        }
        let completed = api_response.status == "completed";
        let parsed = parse_responses_api(api_response);
        if completed
            && let Some(pending) = pending
            && let Some(id) = raw["id"].as_str()
        {
            self.continuations.remember(pending, id, &parsed);
        }
        Ok(parsed)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let mut body = self.build_request(messages, tools, config);
        body["stream"] = true.into();
        let (response, mut pending) = self.send_request(&body, config, true).await?;
        let continuations = Arc::clone(&self.continuations);
        let sse_stream =
            crate::sse::parse_sse_response(response).chain(futures::stream::once(async {
                crate::sse::SseEvent {
                    event: None,
                    data: r#"{"type":"octos.stream_end"}"#.into(),
                }
            }));
        let event_stream = sse_stream
            .scan(ResponsesStreamState::default(), move |state, event| {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&event.data)
                    && !state.terminal
                    && data["type"] == "response.completed"
                    && data["response"]["status"] == "completed"
                    && let Some(pending) = pending.take()
                    && let Some(id) = data["response"]["id"].as_str()
                    && let Ok(response) =
                        serde_json::from_value::<ResponsesApiResponse>(data["response"].clone())
                {
                    continuations.remember(pending, id, &parse_responses_api(response));
                }
                futures::future::ready(Some(map_responses_sse(state, &event)))
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

    fn api_style(&self) -> Option<crate::provider::ApiStyle> {
        Some(crate::provider::ApiStyle::OpenAiResponses)
    }

    fn provider_metadata(&self) -> ProviderMetadata {
        let endpoint = if self.base_url != "https://api.openai.com/v1" {
            endpoint_label_from_base_url(&self.base_url)
        } else {
            None
        };
        ProviderMetadata::new("openai", self.model.clone(), endpoint)
    }
}

// ---- Input message building ----

fn build_input_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut input = Vec::new();
    // Media a current-batch tool handed the model: a function_call_output
    // is text only, so it goes out as one user item after the batch's
    // outputs (the Responses API allows consecutive user items), built at
    // wire time, never persisted.
    let mut pending_media: Vec<serde_json::Value> = Vec::new();
    let mut pending_notes: Vec<String> = Vec::new();
    for (index, msg) in messages.iter().enumerate() {
        // Skip empty assistant messages with no tool calls
        if msg.role == MessageRole::Assistant
            && msg.content.is_empty()
            && msg.tool_calls.as_ref().is_none_or(|tc| tc.is_empty())
        {
            continue;
        }
        if msg.role != MessageRole::Tool && !pending_media.is_empty() {
            input.push(media_item(&mut pending_media, &mut pending_notes));
        }
        if msg.role == MessageRole::Tool {
            let raw_id = msg.tool_call_id.as_deref().unwrap_or("unknown");
            let call_id = normalize_call_id(raw_id);
            // No `input_video` item exists on this API: video is named only.
            let shown = crate::tool_media::for_tool_row(messages, index, false, true);
            let mut output = crate::tool_media::with_note(&msg.content, shown.note.as_deref());
            let mut rendered = Vec::new();
            for path in &shown.images {
                match crate::vision::encode_image(path) {
                    Ok((mime, data)) => {
                        pending_media.push(serde_json::json!({
                            "type": "input_image",
                            "image_url": format!("data:{mime};base64,{data}"),
                        }));
                        rendered.push(path.clone());
                    }
                    Err(_) => {
                        output = crate::tool_media::with_note(
                            &output,
                            Some(&crate::tool_media::unreadable_note(path)),
                        )
                    }
                }
            }
            if !rendered.is_empty() {
                pending_notes.push(crate::tool_media::shown_note(&call_id, &rendered));
            }
            input.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            }));
            continue;
        }
        build_input_items(msg, &mut input);
    }
    if !pending_media.is_empty() {
        input.push(media_item(&mut pending_media, &mut pending_notes));
    }
    input
}

/// The user item that carries a tool batch's images (see `build_input_messages`).
fn media_item(media: &mut Vec<serde_json::Value>, notes: &mut Vec<String>) -> serde_json::Value {
    let mut parts = std::mem::take(media);
    parts.push(
        serde_json::json!({ "type": "input_text", "text": std::mem::take(notes).join("\n") }),
    );
    serde_json::json!({ "role": "user", "content": parts })
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
    /// Automatic prompt-cache breakdown. `cached_tokens` counts the portion
    /// of `input_tokens` served from OpenAI's cache (INCLUDED in
    /// `input_tokens`, unlike Anthropic's disjoint accounting).
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
    #[serde(default)]
    input_tokens_details: InputTokensDetails,
}

#[derive(Default, Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
    #[serde(default)]
    cache_write_tokens: u32,
}

#[derive(Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
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
                            content.get_or_insert_with(String::new).push_str(&text);
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
                    reasoning_content
                        .get_or_insert_with(String::new)
                        .push_str(&text);
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

    // The Responses API reports cached tokens INSIDE input_tokens; the
    // TokenUsage contract is disjoint (Anthropic-style: total prompt =
    // input + cache_read), so subtract at the boundary — the same
    // normalization the chat-completions parser applies.
    let cached = resp
        .usage
        .input_tokens_details
        .as_ref()
        .map(|d| d.cached_tokens)
        .unwrap_or(0);
    let written = resp
        .usage
        .input_tokens_details
        .as_ref()
        .map(|d| d.cache_write_tokens)
        .unwrap_or(0);
    let reasoning_tokens = resp
        .usage
        .output_tokens_details
        .as_ref()
        .map(|d| d.reasoning_tokens)
        .unwrap_or(0);

    ChatResponse {
        content,
        reasoning_content,
        tool_calls,
        stop_reason,
        usage: TokenUsage {
            input_tokens: resp.usage.input_tokens.saturating_sub(cached).saturating_sub(written),
            cache_write_tokens: written,
            output_tokens: resp.usage.output_tokens,
            reasoning_tokens,
            cache_read_tokens: cached,
            ..Default::default()
        },
        provider_index: None,
    }
}

// ---- Streaming SSE ----

#[derive(Default)]
struct ResponsesStreamState {
    tool_calls: Vec<(String, String, String)>, // (call_id, name, args_buffer)
    input_tokens: u32,
    terminal: bool,
    tool_items: std::collections::HashMap<String, usize>,
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
        "response.reasoning.delta"
        | "response.reasoning_text.delta"
        | "response.reasoning_summary_text.delta" => {
            let delta = data["delta"].as_str().unwrap_or("");
            if delta.is_empty() {
                vec![]
            } else {
                vec![StreamEvent::ReasoningDelta(delta.to_string())]
            }
        }

        "response.output_item.added" if data["item"]["type"] == "function_call" => {
            let item = &data["item"];
            let id = item["call_id"].as_str().unwrap_or("").to_string();
            let name = item["name"].as_str().unwrap_or("").to_string();
            let index = state.tool_calls.len();
            state
                .tool_items
                .insert(item["id"].as_str().unwrap_or("").into(), index);
            state
                .tool_calls
                .push((id.clone(), name.clone(), String::new()));
            vec![StreamEvent::ToolCallDelta {
                index,
                id: Some(id),
                name: Some(name),
                arguments_delta: String::new(),
            }]
        }
        // Function call start (legacy compatible servers)
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
            let idx = data["item_id"]
                .as_str()
                .and_then(|id| state.tool_items.get(id))
                .copied()
                .unwrap_or_else(|| state.tool_calls.len().saturating_sub(1));
            if let Some(call) = state.tool_calls.get_mut(idx) {
                call.2.push_str(&delta);
            }
            vec![StreamEvent::ToolCallDelta {
                index: idx,
                id: None,
                name: None,
                arguments_delta: delta,
            }]
        }

        // Response completed — emit usage + done
        "response.completed" | "response.incomplete" => {
            state.terminal = true;
            let usage = &data["response"]["usage"];
            let input = usage["input_tokens"].as_u64().unwrap_or(0) as u32;
            let output = usage["output_tokens"].as_u64().unwrap_or(0) as u32;
            // Cached tokens are reported INSIDE input_tokens; normalize to
            // the disjoint TokenUsage contract (total = input + cache_read),
            // same as the non-streaming parse.
            let cached = usage["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0) as u32;
            let reasoning = usage["output_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .unwrap_or(0) as u32;

            let cached = usage["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0) as u32;
            let written = usage["input_tokens_details"]["cache_write_tokens"]
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
                    input_tokens: input.saturating_sub(cached).saturating_sub(written),
                    cache_write_tokens: written,
                    output_tokens: output,
                    reasoning_tokens: reasoning,
                    cache_read_tokens: cached,
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

        "response.failed" | "error" => {
            state.terminal = true;
            vec![StreamEvent::Error(data.to_string())]
        }
        "octos.stream_end" if !state.terminal => {
            state.terminal = true;
            vec![StreamEvent::Error(
                "Responses stream ended before a terminal event".into(),
            )]
        }
        _ if data.get("error").is_some() => {
            state.terminal = true;
            vec![StreamEvent::Error(data.to_string())]
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
    use crate::config::PromptCacheContext;
    use octos_core::{Message, MessageRole};

    /// A tool loop whose tool handed the model an image: user, assistant
    /// tool call, tool row with the PNG on its media.
    fn media_loop(dir: &std::path::Path) -> (Vec<Message>, String) {
        let png = dir.join("grab.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n").unwrap();
        let path = png.to_string_lossy().into_owned();
        let mk = |role: MessageRole, content: &str| Message {
            role,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        let mut assistant = mk(MessageRole::Assistant, "");
        assistant.tool_calls = Some(vec![octos_core::ToolCall {
            id: "call_1".into(),
            name: "view_image".into(),
            arguments: serde_json::json!({"path": "grab.png"}),
            metadata: None,
        }]);
        let mut tool = mk(MessageRole::Tool, "{\"format\":\"png\"}");
        tool.tool_call_id = Some("call_1".into());
        tool.media = vec![path.clone()];
        (
            vec![mk(MessageRole::User, "look at grab.png"), assistant, tool],
            path,
        )
    }

    #[test]
    fn should_render_tool_media_as_a_user_item_after_the_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let (msgs, _) = media_loop(dir.path());
        let items = build_input_messages(&msgs);
        let kinds: Vec<String> = items
            .iter()
            .map(|i| {
                i["type"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| i["role"].as_str().unwrap().to_string())
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["user", "function_call", "function_call_output", "user"],
            "{items:?}"
        );
        let parts = items[3]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "input_image");
        assert!(
            parts[1]["text"].as_str().unwrap().contains("fc_")
                || parts[1]["text"].as_str().unwrap().contains("call_1")
        );
    }

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
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        let mut items = Vec::new();
        build_input_items(&m, &mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"].as_str(), Some("function_call_output"));
        assert_eq!(items[0]["call_id"].as_str(), Some("fc_123"));
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
            client_message_id: None,
            thread_id: None,
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
        assert_eq!(items[1]["call_id"].as_str(), Some("fc_1"));
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
    fn prompt_cache_key_is_capability_gated_to_official_responses_endpoint() {
        let config = ChatConfig {
            prompt_cache_context: Some(PromptCacheContext {
                affinity_key: "octos-stable-affinity".into(),
                epoch_id: "epoch-1".into(),
                stable_prefix_hash: "stable-1".into(),
                semantic_boundaries: vec![],
            }),
            ..Default::default()
        };
        let messages = vec![msg(MessageRole::User, "hello")];

        let official = OpenAIResponsesProvider::new("test-key", "gpt-5");
        let official_body = official.build_request(&messages, &[], &config);
        assert_eq!(official_body["prompt_cache_key"], "octos-stable-affinity");

        let compatible = OpenAIResponsesProvider::new("test-key", "gpt-5")
            .with_base_url("https://compatible.example/v1");
        let compatible_body = compatible.build_request(&messages, &[], &config);
        assert!(
            compatible_body.get("prompt_cache_key").is_none(),
            "reserved OpenAI fields must stay absent on unknown compatibility endpoints"
        );

        let explicitly_capable = compatible.with_prompt_cache_affinity(true);
        let opted_in_body = explicitly_capable.build_request(&messages, &[], &config);
        assert_eq!(opted_in_body["prompt_cache_key"], "octos-stable-affinity");
    }

    /// The Responses lane used to label manifests `openai-responses` while
    /// its metadata (and therefore every usage row) said `openai`; nothing
    /// ever correlated. Both must carry the same lane identity.
    #[test]
    fn should_build_responses_manifest_with_the_same_provider_label_as_provider_metadata() {
        let provider = OpenAIResponsesProvider::new("test-key", "gpt-4.1");
        let config = ChatConfig::default();
        let messages = vec![msg(MessageRole::User, "hello")];
        let body = provider.build_request(&messages, &[], &config);
        let manifest = provider.prompt_cache_input_manifest(&body, &config);
        let metadata = provider.provider_metadata();
        assert_eq!(manifest.provider, metadata.provider);
        assert_eq!(manifest.model, metadata.model);
        assert_eq!(
            manifest.provider,
            provider.provider_metadata_for_index(None).provider
        );
    }

    #[test]
    fn should_serialize_responses_tool_choice_only_when_explicit() {
        let provider = OpenAIResponsesProvider::new("test-key", "gpt-4.1");
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let messages = vec![msg(MessageRole::User, "hello")];
        let auto = provider.build_request(&messages, &tools, &ChatConfig::default());
        assert!(auto.get("tool_choice").is_none(), "{auto}");
        let none = ChatConfig {
            tool_choice: crate::ToolChoice::None,
            ..Default::default()
        };
        assert_eq!(
            provider.build_request(&messages, &tools, &none)["tool_choice"],
            "none"
        );
        assert!(
            provider
                .build_request(&messages, &[], &none)
                .get("tool_choice")
                .is_none()
        );
    }

    #[test]
    fn prompt_cache_manifest_uses_final_responses_shape_and_redacts_content() {
        let provider = OpenAIResponsesProvider::new("test-key", "gpt-4.1");
        let tools = vec![ToolSpec {
            name: "private_tool".into(),
            description: "tool description secret".into(),
            input_schema: serde_json::json!({"type": "object", "secret": "schema secret"}),
        }];
        let config = ChatConfig {
            prompt_cache_context: Some(PromptCacheContext {
                affinity_key: "session-affinity".into(),
                epoch_id: "epoch-9".into(),
                stable_prefix_hash: "canonical-hash".into(),
                semantic_boundaries: vec![],
            }),
            ..Default::default()
        };
        let first_messages = vec![
            msg(MessageRole::System, "system prompt secret"),
            msg(MessageRole::User, "first user secret"),
        ];
        let next_messages = vec![
            msg(MessageRole::System, "system prompt secret"),
            msg(MessageRole::User, "first user secret"),
            msg(MessageRole::Assistant, "assistant suffix secret"),
        ];
        let first_body = provider.build_request(&first_messages, &tools, &config);
        let mut next_body = provider.build_request(&next_messages, &tools, &config);
        next_body["stream"] = true.into();

        let first_manifest = provider.prompt_cache_input_manifest(&first_body, &config);
        let next_manifest = provider.prompt_cache_input_manifest(&next_body, &config);
        let comparison = first_manifest.compare_prefix(&next_manifest);

        assert!(comparison.compatible_route);
        assert!(comparison.stable_prefix_matches);
        assert_eq!(comparison.conversation_prefix_segments, 1);
        // The manifest carries the lane identity the metadata reports, so
        // usage rows correlate with it.
        assert_eq!(
            first_manifest.provider,
            provider.provider_metadata().provider
        );
        assert_eq!(first_manifest.epoch_id.as_deref(), Some("epoch-9"));
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
    fn test_build_request_reasoning_effort() {
        let provider = OpenAIResponsesProvider::new("test-key", "o3");
        let messages = vec![msg(MessageRole::User, "think")];
        let mut config = ChatConfig::default();
        config.reasoning_effort = Some(crate::config::ReasoningEffort::High);
        let request = provider.build_request(&messages, &[], &config);

        assert_eq!(request["reasoning"]["effort"].as_str(), Some("high"));
    }

    #[test]
    fn should_emit_none_when_reasoning_is_disabled() {
        let provider = OpenAIResponsesProvider::new("test-key", "gpt-5");
        let messages = vec![msg(MessageRole::User, "answer directly")];
        let effort = serde_json::from_value(serde_json::json!("none"))
            .expect("none should disable reasoning");
        let config = ChatConfig {
            reasoning_effort: Some(effort),
            ..Default::default()
        };
        let request = provider.build_request(&messages, &[], &config);

        assert_eq!(request["reasoning"]["effort"].as_str(), Some("none"));
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
                input_tokens_details: InputTokensDetails::default(),
                input_tokens: 10,
                output_tokens: 5,
                input_tokens_details: None,
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
                input_tokens_details: InputTokensDetails::default(),
                input_tokens: 20,
                output_tokens: 30,
                input_tokens_details: None,
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
    fn test_parse_response_cached_tokens_normalized_to_disjoint_accounting() {
        // The Responses API reports the cached portion INSIDE input_tokens;
        // the TokenUsage contract is disjoint, so the total prompt is
        // input + cache_read — same normalization as the chat-completions
        // parser. Deserialize the full wire body so the serde shape is
        // covered, not just the conversion.
        let resp: ResponsesApiResponse = serde_json::from_value(serde_json::json!({
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "Hello!" }]
            }],
            "status": "completed",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 5,
                "input_tokens_details": { "cached_tokens": 75 }
            }
        }))
        .unwrap();
        let result = parse_responses_api(resp);
        assert_eq!(result.usage.input_tokens, 25);
        assert_eq!(result.usage.cache_read_tokens, 75);
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
    fn test_sse_completed_cached_tokens_normalized_to_disjoint_accounting() {
        // Cached tokens arrive INSIDE input_tokens; the streamed Usage event
        // must normalize to the disjoint contract exactly like the
        // non-streaming parse (total = input + cache_read).
        let mut state = ResponsesStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 100, "output_tokens": 50, "input_tokens_details": {"cached_tokens": 75}}}}"#.into(),
        };
        let events = map_responses_sse(&mut state, &event);
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 25);
                assert_eq!(u.cache_read_tokens, 75);
            }
            _ => panic!("expected Usage"),
        }
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

#[cfg(test)]
mod lane_attributed_operational_errors {
    use octos_core::Message;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::OpenAIResponsesProvider;
    use crate::config::ChatConfig;
    use crate::provider::LlmProvider;
    use crate::provider::test_lanes::assert_error_names_lane;

    #[tokio::test]
    async fn should_name_lane_and_api_style_when_response_body_is_malformed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json{"))
            .mount(&server)
            .await;
        let provider = OpenAIResponsesProvider::new("key", "gpt-5").with_base_url(server.uri());
        let err = provider
            .chat(&[Message::user("hi")], &[], &ChatConfig::default())
            .await
            .unwrap_err();
        assert_error_names_lane(
            &err,
            "openai/gpt-5",
            "api_style=openai_responses",
            &["OpenAI Responses API response"],
        );
    }
}
