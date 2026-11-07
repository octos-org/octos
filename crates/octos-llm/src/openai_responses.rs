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
        self.prompt_cache_affinity = is_official_openai_base_url(&base_url);
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
        let input = build_input_messages(messages, config.media_scope_root.as_deref());

        let mut body = serde_json::json!({
            "model": &self.model,
            "input": input,
        });

        if let Some(max) = config.max_tokens {
            body["max_output_tokens"] = max.into();
        }
        if !is_official_openai_base_url(&self.base_url) {
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
        let parsed = parse_responses_api(api_response, &self.model)?;
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
        let model = self.model.clone();
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
                    && let Ok(parsed) = parse_responses_api(response, &model)
                {
                    continuations.remember(pending, id, &parsed);
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
        let endpoint = if !is_official_openai_base_url(&self.base_url) {
            endpoint_label_from_base_url(&self.base_url)
        } else {
            None
        };
        ProviderMetadata::new("openai", self.model.clone(), endpoint)
    }
}

// ---- Input message building ----

fn build_input_messages(
    messages: &[Message],
    scope_root: Option<&std::path::Path>,
) -> Vec<serde_json::Value> {
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
                match crate::vision::encode_image(path, scope_root) {
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
        build_input_items(msg, &mut input, scope_root);
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
fn build_input_items(
    msg: &Message,
    out: &mut Vec<serde_json::Value>,
    scope_root: Option<&std::path::Path>,
) {
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
                "content": build_user_content(msg, scope_root),
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

fn build_user_content(msg: &Message, scope_root: Option<&std::path::Path>) -> serde_json::Value {
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
        if let Ok((mime, data)) = crate::vision::encode_image(path, scope_root) {
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
}

#[derive(Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
    #[serde(default)]
    cache_write_tokens: u32,
}

#[derive(Deserialize)]
struct OutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

fn parse_responses_api(resp: ResponsesApiResponse, model: &str) -> Result<ChatResponse> {
    let truncated = resp.status == "incomplete";
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
                let parsed_args = serde_json::from_str(&arguments).map_err(|error| {
                    let detail = format!(
                        "{} malformed tool arguments: {error}",
                        crate::provider::operational_error_message(
                            crate::provider::OperationalStage::ParseResponse,
                            "openai",
                            model,
                            crate::provider::ApiStyle::OpenAiResponses,
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

    Ok(ChatResponse {
        content,
        reasoning_content,
        tool_calls,
        stop_reason,
        usage: TokenUsage {
            input_tokens: resp
                .usage
                .input_tokens
                .saturating_sub(cached)
                .saturating_sub(written),
            cache_write_tokens: written,
            output_tokens: resp.usage.output_tokens,
            reasoning_tokens,
            cache_read_tokens: cached,
            ..Default::default()
        },
        provider_index: None,
    })
}

// ---- Streaming SSE ----

#[derive(Default)]
struct ResponsesStreamState {
    tool_calls: Vec<(String, String, String)>, // (call_id, name, args_buffer)
    input_tokens: u32,
    terminal: bool,
    tool_items: std::collections::HashMap<String, usize>,
}

/// Whether `base_url` is the official OpenAI endpoint, ignoring a trailing
/// slash (matching the custom-endpoint check in `octos chat`'s wiring).
fn is_official_openai_base_url(base_url: &str) -> bool {
    base_url.trim_end_matches('/') == "https://api.openai.com/v1"
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
        Err(_) if state.terminal => return vec![],
        Err(e) => {
            return vec![StreamEvent::Error(format!(
                "invalid OpenAI Responses SSE JSON: {e}"
            ))];
        }
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

        // Like the stream-end sentinel, an error-shaped event after the
        // stream already reached a terminal (e.g. `response.completed`) must
        // not surface a synthetic error after Usage/Done.
        "response.failed" | "error" if !state.terminal => {
            state.terminal = true;
            vec![StreamEvent::Error(data.to_string())]
        }
        "octos.stream_end" if !state.terminal => {
            state.terminal = true;
            vec![StreamEvent::Error(
                "Responses stream ended before a terminal event".into(),
            )]
        }
        _ if !state.terminal && data.get("error").is_some() => {
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
    fn should_ignore_error_events_when_stream_already_terminal() {
        let mut state = ResponsesStreamState::default();
        let completed = crate::sse::SseEvent {
            event: None,
            data: r#"{"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 1, "output_tokens": 1}}}"#.into(),
        };
        assert_eq!(map_responses_sse(&mut state, &completed).len(), 2);
        for late in [
            r#"{"type": "response.failed", "response": {"status": "failed"}}"#,
            r#"{"type": "error", "message": "late"}"#,
            r#"{"type": "something.else", "error": {"message": "late"}}"#,
            r#"{"type": "octos.stream_end"}"#,
            "{not-json",
        ] {
            let event = crate::sse::SseEvent {
                event: None,
                data: late.into(),
            };
            assert!(
                map_responses_sse(&mut state, &event).is_empty(),
                "late event must not surface after the terminal: {late}"
            );
        }
    }

    #[test]
    fn should_still_surface_error_events_before_a_terminal() {
        for early in [
            r#"{"type": "response.failed", "response": {"status": "failed"}}"#,
            r#"{"type": "error", "message": "boom"}"#,
            r#"{"type": "something.else", "error": {"message": "boom"}}"#,
        ] {
            let mut state = ResponsesStreamState::default();
            let event = crate::sse::SseEvent {
                event: None,
                data: early.into(),
            };
            let events = map_responses_sse(&mut state, &event);
            assert!(
                matches!(events.as_slice(), [StreamEvent::Error(_)]),
                "{early}: {events:?}"
            );
            assert!(state.terminal);
        }
    }
}

#[cfg(test)]
mod lane_attributed_operational_errors {}
