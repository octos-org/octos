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
use crate::provider::{LlmProvider, endpoint_label_from_base_url};
use crate::sse::SseEvent;
use crate::types::{
    ChatResponse, ChatStream, ProviderMetadata, StopReason, StreamEvent, TokenUsage, ToolSpec,
};

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

    /// How this model accepts a reasoning/thinking control on the chat path.
    /// Translates `ChatConfig::reasoning_effort` into request fields; `None`
    /// (default) emits nothing, so it is a no-op for non-thinking models.
    #[serde(default)]
    pub reasoning_style: ReasoningStyle,
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
            reasoning_style: ReasoningStyle::None,
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
            is_o_series || m.starts_with("gpt-5") || m.contains("kimi-k2") || m == "gpt-4.1-nano";

        // Vision capability is NO LONGER inferred from the model name. The old
        // allow/deny list wrongly stripped images from vision-capable models —
        // kimi-k2.5/k2.6 are natively multimodal, moonshot/minimax/deepseek
        // also ship vision variants (deepseek-v4/VL), and the SAME model name
        // can front either a vision endpoint (e.g. `moonshot@api/kimi-k2.6`)
        // or a proxy that rejects `image_url` parts (e.g. `moonshot@autodl`).
        // A name heuristic cannot tell those apart, so it silently dropped
        // images users uploaded to vision-capable models.
        //
        // Instead we ATTEMPT images for user uploads and rely on the graceful
        // image-modality fallback in `chat()` / `chat_stream()`: if the
        // endpoint returns the `400 ... incorrect modal "image"` (or similar)
        // error, we retry the request once text-only. `lacks_vision` remains a
        // config-overridable field so an operator CAN pre-strip for a known
        // text-only endpoint and skip the one doomed attempt — it just is not
        // auto-asserted from the model name.
        let lacks_vision = false;

        // Reasoning-control style on the chat/completions path. DeepSeek V4
        // (incl. `deepseek-reasoner`, a V4-Flash thinking alias) wants
        // `reasoning_effort` + a `thinking` toggle; OpenAI reasoning models and
        // grok-4.x take a plain `reasoning_effort`. Everything else emits nothing.
        // Effort/thinking is only EMITTED when an operator sets `reasoning_effort`
        // (opt-in), and the style is config-overridable per route — important
        // because the same `deepseek-v4` name fronts endpoints that differ
        // (api.deepseek.com accepts it; nvidia/vllm may not), same caveat as
        // `lacks_vision`. `grok` is narrowed to `grok-4` since older Grok
        // families can reject `reasoning_effort`.
        let reasoning_style = if m.contains("deepseek-v4") || m.contains("deepseek-reasoner") {
            ReasoningStyle::EffortAndThinkingToggle
        } else if m.starts_with("grok-4") || is_o_series || m.starts_with("gpt-5") {
            ReasoningStyle::Effort
        } else {
            ReasoningStyle::None
        };

        Self {
            uses_completion_tokens,
            fixed_temperature,
            lacks_vision,
            merge_system_messages: true,
            reasoning_style,
        }
    }
}

/// How a model on the OpenAI-compatible chat/completions path accepts a
/// reasoning/thinking control. Used to translate the provider-agnostic
/// [`ChatConfig::reasoning_effort`](crate::config::ChatConfig) into the right
/// request fields. The SAME model name can front endpoints with different
/// support (cf. `lacks_vision`), so this is config-overridable via `ModelHints`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningStyle {
    /// No reasoning control emitted on the chat path (default — backward compatible).
    #[default]
    None,
    /// Top-level `reasoning_effort: "low"|"medium"|"high"` — OpenAI chat-completions
    /// reasoning models (o-series / gpt-5) and xAI Grok.
    Effort,
    /// `reasoning_effort` plus `thinking: {"type": "enabled"}` — DeepSeek V4.
    EffortAndThinkingToggle,
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
        let url = base_url.into();
        // If using a non-default base URL, tag the provider_label to distinguish
        // it in the adaptive router (e.g., "moonshot@autodl" vs "moonshot").
        if url != "https://api.openai.com/v1" {
            if let Some(domain) = url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
            {
                // Use the domain name (minus TLD) as the tag.
                // "www.autodl.art" → "autodl", "api.moonshot.ai" → "api"
                let parts: Vec<&str> = domain.split('.').collect();
                let short = if parts.len() >= 2 && parts[0] == "www" {
                    parts[1] // skip "www", use "autodl"
                } else {
                    parts[0] // use "api" from "api.moonshot.ai"
                };
                if !self.provider_label.contains('@') {
                    self.provider_label = format!("{}@{}", self.provider_label, short);
                }
            }
        }
        // The DeepSeek `thinking` toggle is specific to DeepSeek's official API.
        // The same `deepseek-v4` model name fronted by other endpoints
        // (nvidia/vllm/wisemodel) uses different — or no — reasoning controls, so
        // don't emit DeepSeek-specific fields there by default. Operators opt in
        // per route via `model_hints` (with_hints, applied after this, still wins).
        if self.hints.reasoning_style == ReasoningStyle::EffortAndThinkingToggle
            && !url.contains("api.deepseek.com")
        {
            self.hints.reasoning_style = ReasoningStyle::None;
        }
        self.base_url = url;
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

    /// POST a non-streaming chat request. Factored so the graceful
    /// image-modality fallback can re-send a rebuilt (text-only) request
    /// without duplicating the wire setup.
    async fn post_chat(&self, request: &OpenAIRequest<'_>) -> Result<reqwest::Response> {
        self.client
            .post(format!("{}/chat/completions", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
            ))
            .json(request)
            .send()
            .await
            .wrap_err("failed to send request to OpenAI")
    }

    /// POST a streaming chat request (adds `stream` + `stream_options`).
    /// Factored for the same image-modality fallback as [`Self::post_chat`].
    async fn post_chat_stream(&self, request: &OpenAIRequest<'_>) -> Result<reqwest::Response> {
        let mut body =
            serde_json::to_value(request).wrap_err("failed to serialize OpenAI request")?;
        let obj = body
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("failed to build OpenAI request body"))?;
        obj.insert("stream".into(), true.into());
        obj.insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );
        self.client
            .post(format!("{}/chat/completions", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send streaming request to OpenAI")
    }

    /// Build the shared request struct used by both chat() and chat_stream().
    ///
    /// `force_text_only` strips user-uploaded images even when the model is
    /// treated as vision-capable. It is set on the retry leg of the graceful
    /// image-modality fallback (see `chat()` / `chat_stream()`): when an
    /// endpoint rejects `image_url` parts with a 400, the request is rebuilt
    /// text-only so the turn still proceeds.
    fn build_request<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
        config: &ChatConfig,
        force_text_only: bool,
    ) -> OpenAIRequest<'a> {
        // Effective content hints: honour the configured `lacks_vision`, and
        // additionally strip images on the text-only retry leg.
        let mut content_hints = self.hints.clone();
        content_hints.lacks_vision = content_hints.lacks_vision || force_text_only;
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
                // Kimi-k2 (and similar thinking models) require reasoning_content
                // to be present (even empty) on ALL assistant messages when thinking
                // is enabled. When omitted, the API returns 400 "reasoning_content
                // is missing in assistant tool call message".
                // Only synthesize a stub for models that actually need it (detected
                // via fixed_temperature + model name containing "kimi-k2").
                // NOTE: deepseek-v4's official API was verified NOT to enforce this
                // (multi-round-with-tools returns 200 without reasoning_content), and
                // a "." stub could break non-official deepseek-v4 endpoints
                // (nvidia/vllm) that don't expect the field — so it is NOT stubbed.
                // Real reasoning_content the model returns is still round-tripped below.
                let needs_reasoning_stub =
                    self.hints.fixed_temperature && self.model.to_lowercase().contains("kimi-k2");
                let reasoning = match m.reasoning_content.as_deref() {
                    Some(r) if !r.is_empty() => Some(r),
                    _ if role == "assistant" && needs_reasoning_stub => Some("."),
                    _ => None,
                };

                OpenAIMessage {
                    role,
                    content: build_openai_content(m, &content_hints),
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

        // Translate the provider-agnostic `reasoning_effort` into the chat-path
        // request fields the model's ReasoningStyle expects. Emitted only when
        // an effort is configured AND the model declares a non-None style, so
        // it stays a no-op for models/endpoints that don't accept it.
        let (reasoning_effort, thinking) =
            match (config.reasoning_effort, self.hints.reasoning_style) {
                (Some(effort), style) if style != ReasoningStyle::None => {
                    use crate::config::ReasoningEffort as RE;
                    let effort_str = match (effort, style) {
                        (RE::Low, _) => "low",
                        (RE::Medium, _) => "medium",
                        (RE::High, _) => "high",
                        // DeepSeek V4 accepts "max"; Effort-style providers
                        // (OpenAI/Grok) have no max tier, so clamp to "high".
                        (RE::Max, ReasoningStyle::EffortAndThinkingToggle) => "max",
                        (RE::Max, _) => "high",
                    };
                    let thinking = if style == ReasoningStyle::EffortAndThinkingToggle {
                        Some(serde_json::json!({ "type": "enabled" }))
                    } else {
                        None
                    };
                    (Some(effort_str), thinking)
                }
                _ => (None, None),
            };

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
            reasoning_effort,
            thinking,
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
        let request = self.build_request(messages, tools, config, false);
        let mut response = self.post_chat(&request).await?;

        // Graceful image-modality fallback: a vision-capable model behind a
        // proxy that rejects `image_url` parts (or a genuinely text-only
        // model) returns a 400 when images are present. Retry once text-only
        // so the turn proceeds instead of erroring — the agent can still
        // `read_file` the attachment via the media note. See
        // `is_image_modality_error` / `ModelHints::detect`.
        if response.status().as_u16() == 400 && request_has_user_images(messages, &self.hints) {
            let body = response.text().await.unwrap_or_default();
            if is_image_modality_error(&body)
                && crate::current_llm_call_policy() != crate::LlmCallPolicy::FailFast
            {
                tracing::warn!(
                    provider = %self.provider_label,
                    model = %self.model,
                    "endpoint rejected image content (400); retrying text-only"
                );
                let retry = self.build_request(messages, tools, config, true);
                response = self.post_chat(&retry).await?;
            } else {
                let body = crate::provider::truncate_error_body(&body);
                return Err(crate::error::LlmError::from_status_with_label(
                    400,
                    &body,
                    format!("{}/{}", self.provider_label, self.model),
                )
                .into());
            }
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            // Route through LlmError so the loop-boundary classifier
            // (`HarnessError::classify_report`) can downcast and pick the
            // correct user-facing variant (auth / quota / bad-request /
            // rate-limited / server) instead of falling through to
            // Internal/Bug. The truncated body is preserved so the
            // operator sees the provider's actual error payload.
            //
            // Codex round-2 MINOR: thread the provider_label so the
            // operator sees e.g. "minimax/MiniMax-M2.5-highspeed"
            // instead of just "MiniMax-M2.5-highspeed". This is the
            // lane label the AdaptiveRouter and failover ledger use,
            // so the wire envelope can be cross-referenced with the
            // router events.
            let body = crate::provider::truncate_error_body(&body);
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &body,
                format!("{}/{}", self.provider_label, self.model),
            )
            .into());
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
            provider_index: None,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let request = self.build_request(messages, tools, config, false);
        let mut response = self.post_chat_stream(&request).await?;

        // Graceful image-modality fallback (see `chat()`): retry once
        // text-only if the endpoint rejected the image content parts.
        if response.status().as_u16() == 400 && request_has_user_images(messages, &self.hints) {
            let text = response.text().await.unwrap_or_default();
            if is_image_modality_error(&text)
                && crate::current_llm_call_policy() != crate::LlmCallPolicy::FailFast
            {
                tracing::warn!(
                    provider = %self.provider_label,
                    model = %self.model,
                    "endpoint rejected image content (400); retrying text-only (stream)"
                );
                let retry = self.build_request(messages, tools, config, true);
                response = self.post_chat_stream(&retry).await?;
            } else {
                let body = crate::provider::truncate_error_body(&text);
                return Err(crate::error::LlmError::from_status_with_label(
                    400,
                    &body,
                    format!("{}/{}", self.provider_label, self.model),
                )
                .into());
            }
        }

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let body = crate::provider::truncate_error_body(&text);
            // Codex round-2 MINOR: see chat() — thread provider_label so
            // the streaming error path identifies the lane the same way.
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &body,
                format!("{}/{}", self.provider_label, self.model),
            )
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
        &self.provider_label
    }

    fn provider_metadata(&self) -> ProviderMetadata {
        let (provider, tagged_endpoint) = self
            .provider_label
            .split_once('@')
            .map(|(provider, endpoint)| (provider.to_string(), Some(endpoint.to_string())))
            .unwrap_or_else(|| (self.provider_label.clone(), None));
        let endpoint = match tagged_endpoint.as_deref() {
            Some("api") | None => None,
            Some(_) => endpoint_label_from_base_url(&self.base_url).or(tagged_endpoint),
        };
        ProviderMetadata::new(provider, self.model.clone(), endpoint)
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
    /// Reasoning effort ("low"|"medium"|"high"), emitted only for models whose
    /// `ReasoningStyle` accepts it (OpenAI reasoning models, Grok, DeepSeek V4).
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    /// DeepSeek V4 thinking toggle (`{"type": "enabled"}`); other styles omit it.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<serde_json::Value>,
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

/// Whether a provider 400 means "this endpoint/model can't accept image
/// content parts" — as opposed to any other bad request. Vision-capable
/// models fronted by a text-only proxy, and genuinely text-only models,
/// return these when `image_url` parts are present. Matched case-insensitively
/// against the (truncated) error body.
///
/// Examples seen in production (mini3 `dspfac`): kimi via the autodl proxy
/// returns `400 InvalidParameter: incorrect modal "image" was entered, which
/// may not be supported by the model`.
fn is_image_modality_error(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("incorrect modal")
        || (b.contains("modal") && b.contains("image"))
        || b.contains("does not support image")
        || b.contains("image input is not supported")
        || b.contains("not support vision")
        || b.contains("vision is not supported")
        || (b.contains("image_url") && b.contains("not support"))
}

/// Whether the request carries at least one user-uploaded image we would have
/// inlined (i.e. images are not already stripped by the configured
/// `lacks_vision`). Decides whether a 400 is worth retrying text-only.
fn request_has_user_images(messages: &[Message], hints: &ModelHints) -> bool {
    !hints.lacks_vision
        && messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.media.iter().any(|p| vision::is_image(p)))
}

fn build_openai_content(msg: &Message, hints: &ModelHints) -> Option<OpenAIContent> {
    // Only inline images on USER messages. Tool outputs (Assistant/Tool
    // role with `media`) are previous-turn artifacts the agent emitted —
    // e.g. `send_file(skill-output/slides/<slug>/output/slide-NN.png)` —
    // and feeding them back into the LLM as `image_url` content on every
    // subsequent turn is both wasteful (~1 MB per slide per call) and
    // wrong: the LLM never asked to see the rendered output, and some
    // providers (kimi-k2.5, deepseek, minimax) reject `image_url` parts
    // outright. Mini3 dspfac slides session 1779130130502-th18yr hit
    // this when generated slide PNGs were re-encoded on every turn.
    //
    // The `read_file` text path still works: the assistant can read the
    // image's bytes if it really needs to inspect them, but the file is
    // not pushed unsolicited into vision content.
    let images: Vec<_> = if hints.lacks_vision || msg.role != MessageRole::User {
        vec![]
    } else {
        msg.media.iter().filter(|p| vision::is_image(p)).collect()
    };

    if images.is_empty() {
        // Build a note for any media the LLM won't see inline:
        // - Non-image files (CSV, PDF, etc.) → include full path so agent can read_file
        // - Images stripped because model lacks vision → include filename
        let non_image_files: Vec<_> = msg.media.iter().filter(|p| !vision::is_image(p)).collect();
        let stripped_images = hints.lacks_vision && msg.media.iter().any(|p| vision::is_image(p));

        let media_note = if !non_image_files.is_empty() || stripped_images {
            let mut parts = Vec::new();
            for path in &non_image_files {
                // Include full path so the agent can use read_file to access it
                parts.push(path.to_string());
            }
            if stripped_images {
                for p in msg.media.iter().filter(|p| vision::is_image(p)) {
                    let name = std::path::Path::new(p)
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone());
                    parts.push(name);
                }
            }
            // Mini5 2026-05-12: phrasing is intentional — earlier
            // wording ("Use read_file to access them.") caused DeepSeek
            // to refuse `/private/var/...` upload paths thinking they
            // were outside the workspace. Stating authorization in the
            // note keeps the model on the happy path.
            Some(format!(
                "[user-uploaded files: {}. These are authenticated attachments — \
                 call read_file with this exact path. The path is whitelisted \
                 even if it lies outside the workspace root.]",
                parts.join(", ")
            ))
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
            // Reasoning/thinking content (kimi-k2.5, o1, etc.). OpenRouter
            // uses `reasoning`; most OpenAI-compatible providers use
            // `reasoning_content`.
            if let Some(reasoning) = choice["delta"]["reasoning_content"]
                .as_str()
                .or_else(|| choice["delta"]["reasoning"].as_str())
            {
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
            client_message_id: None,
            thread_id: None,
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
    fn test_detect_kimi_k25_is_not_pre_stripped() {
        let h = ModelHints::detect("kimi-k2.5");
        assert!(!h.uses_completion_tokens);
        assert!(h.fixed_temperature);
        // Vision is NO LONGER inferred from the model name. Kimi K2.5/K2.6 are
        // natively multimodal; the old heuristic wrongly stripped images from
        // them. The same model name can also front a vision endpoint
        // (moonshot@api) or an image-rejecting proxy (moonshot@autodl), which a
        // name check can't distinguish — so we attempt images and let the
        // graceful image-modality fallback retry text-only if the endpoint 400s.
        assert!(!h.lacks_vision);
    }

    #[test]
    fn test_detect_deepseek_is_not_pre_stripped() {
        // deepseek-chat is text-only, but deepseek-v4/VL are vision; we no
        // longer pre-strip by name. A text-only endpoint that 400s on an image
        // is handled by the image-modality fallback, not a hardcoded flag.
        let h = ModelHints::detect("deepseek-chat");
        assert!(!h.uses_completion_tokens);
        assert!(!h.fixed_temperature);
        assert!(!h.lacks_vision);
    }

    #[test]
    fn test_detect_minimax_is_not_pre_stripped() {
        let h = ModelHints::detect("MiniMax-Text-01");
        assert!(!h.lacks_vision);
        assert!(h.merge_system_messages);
    }

    #[test]
    fn is_image_modality_error_matches_known_provider_400s() {
        // The exact string observed live on mini3 (kimi via the autodl proxy).
        assert!(is_image_modality_error(
            "InvalidParameter: incorrect modal \"image\" was entered, which may not be supported by the model"
        ));
        assert!(is_image_modality_error(
            "This model does not support image input"
        ));
        assert!(is_image_modality_error(
            "vision is not supported by this endpoint"
        ));
        // Unrelated 400s must NOT trigger the text-only retry.
        assert!(!is_image_modality_error("invalid api key"));
        assert!(!is_image_modality_error("context length exceeded"));
        assert!(!is_image_modality_error("rate limit reached"));
    }

    #[test]
    fn request_has_user_images_gates_the_fallback() {
        let img = Message {
            role: MessageRole::User,
            content: "look at this".into(),
            media: vec!["/tmp/pic.png".into()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        let vision = ModelHints::default();
        assert!(request_has_user_images(std::slice::from_ref(&img), &vision));
        // Already configured text-only ⇒ images were never sent ⇒ no retry.
        let text_only = ModelHints {
            lacks_vision: true,
            ..ModelHints::default()
        };
        assert!(!request_has_user_images(
            std::slice::from_ref(&img),
            &text_only
        ));
        // A non-image attachment is not an image-modality concern.
        let mut doc = img.clone();
        doc.media = vec!["/tmp/data.csv".into()];
        assert!(!request_has_user_images(
            std::slice::from_ref(&doc),
            &vision
        ));
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
            reasoning_style: ReasoningStyle::EffortAndThinkingToggle,
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
        assert_eq!(h.reasoning_style, ReasoningStyle::None);
    }

    #[test]
    fn detect_reasoning_style_per_model_family() {
        // DeepSeek V4 (pro + flash, incl. provider-prefixed) -> effort + thinking toggle.
        assert_eq!(
            ModelHints::detect("deepseek-v4-pro").reasoning_style,
            ReasoningStyle::EffortAndThinkingToggle
        );
        assert_eq!(
            ModelHints::detect("deepseek-ai/deepseek-v4-flash").reasoning_style,
            ReasoningStyle::EffortAndThinkingToggle
        );
        // deepseek-reasoner is a V4-Flash thinking alias.
        assert_eq!(
            ModelHints::detect("deepseek-reasoner").reasoning_style,
            ReasoningStyle::EffortAndThinkingToggle
        );
        // grok-4.x + OpenAI reasoning models -> plain reasoning_effort.
        assert_eq!(
            ModelHints::detect("grok-4.3").reasoning_style,
            ReasoningStyle::Effort
        );
        assert_eq!(
            ModelHints::detect("gpt-5.3-codex").reasoning_style,
            ReasoningStyle::Effort
        );
        // Non-thinking / unknown-control models emit nothing. grok-3 is
        // excluded (only grok-4.x is known to accept reasoning_effort).
        for m in [
            "deepseek-chat",
            "MiniMax-M3",
            "kimi-k2.6",
            "gpt-4o",
            "grok-3",
        ] {
            assert_eq!(
                ModelHints::detect(m).reasoning_style,
                ReasoningStyle::None,
                "{m} should not declare a reasoning style"
            );
        }
    }

    #[test]
    fn deepseek_v4_thinking_style_downgraded_off_official_endpoint() {
        // Official endpoint keeps the DeepSeek-specific thinking toggle.
        let official = OpenAIProvider::new("k", "deepseek-v4-pro")
            .with_base_url("https://api.deepseek.com/v1");
        assert_eq!(
            official.hints.reasoning_style,
            ReasoningStyle::EffortAndThinkingToggle
        );
        // The same model name on a non-DeepSeek endpoint must not inherit it.
        let nvidia = OpenAIProvider::new("k", "deepseek-ai/deepseek-v4-pro")
            .with_base_url("https://integrate.api.nvidia.com/v1");
        assert_eq!(nvidia.hints.reasoning_style, ReasoningStyle::None);
        // Explicit config override still wins (with_hints runs after with_base_url).
        let overridden = OpenAIProvider::new("k", "deepseek-ai/deepseek-v4-pro")
            .with_base_url("https://integrate.api.nvidia.com/v1")
            .with_hints(ModelHints {
                reasoning_style: ReasoningStyle::Effort,
                ..Default::default()
            });
        assert_eq!(overridden.hints.reasoning_style, ReasoningStyle::Effort);
    }

    #[test]
    fn build_request_emits_effort_and_thinking_for_deepseek_v4() {
        let p = OpenAIProvider::new("key", "deepseek-v4-pro");
        let cfg = ChatConfig {
            reasoning_effort: Some(crate::config::ReasoningEffort::High),
            ..Default::default()
        };
        let msgs = [msg("hi")];
        let v = serde_json::to_value(p.build_request(&msgs, &[], &cfg, false)).unwrap();
        assert_eq!(v["reasoning_effort"], "high");
        assert_eq!(v["thinking"], serde_json::json!({ "type": "enabled" }));
    }

    #[test]
    fn build_request_maps_max_effort_by_style() {
        let msgs = [msg("hi")];
        let cfg = ChatConfig {
            reasoning_effort: Some(crate::config::ReasoningEffort::Max),
            ..Default::default()
        };
        // deepseek (EffortAndThinkingToggle) emits DeepSeek's real "max".
        let ds = OpenAIProvider::new("k", "deepseek-v4-pro");
        let v = serde_json::to_value(ds.build_request(&msgs, &[], &cfg, false)).unwrap();
        assert_eq!(v["reasoning_effort"], "max");
        // Effort-style providers (grok) have no max tier -> clamp to "high".
        let grok = OpenAIProvider::new("k", "grok-4.3");
        let v2 = serde_json::to_value(grok.build_request(&msgs, &[], &cfg, false)).unwrap();
        assert_eq!(v2["reasoning_effort"], "high");
    }

    #[test]
    fn build_request_emits_only_effort_for_grok() {
        let p = OpenAIProvider::new("key", "grok-4.3");
        let cfg = ChatConfig {
            reasoning_effort: Some(crate::config::ReasoningEffort::Low),
            ..Default::default()
        };
        let msgs = [msg("hi")];
        let v = serde_json::to_value(p.build_request(&msgs, &[], &cfg, false)).unwrap();
        assert_eq!(v["reasoning_effort"], "low");
        assert!(
            v.get("thinking").is_none(),
            "grok must not emit a thinking toggle"
        );
    }

    #[test]
    fn build_request_omits_reasoning_when_unset_or_unsupported() {
        let msgs = [msg("hi")];
        // Effort configured but the model has no reasoning control -> nothing.
        let p = OpenAIProvider::new("key", "deepseek-chat");
        let cfg = ChatConfig {
            reasoning_effort: Some(crate::config::ReasoningEffort::High),
            ..Default::default()
        };
        let v = serde_json::to_value(p.build_request(&msgs, &[], &cfg, false)).unwrap();
        assert!(v.get("reasoning_effort").is_none());
        assert!(v.get("thinking").is_none());

        // Supported model but no effort configured -> nothing.
        let p2 = OpenAIProvider::new("key", "deepseek-v4-pro");
        let cfg2 = ChatConfig {
            reasoning_effort: None,
            ..Default::default()
        };
        let v2 = serde_json::to_value(p2.build_request(&msgs, &[], &cfg2, false)).unwrap();
        assert!(v2.get("reasoning_effort").is_none());
        assert!(v2.get("thinking").is_none());
    }

    #[test]
    fn build_request_does_not_stub_reasoning_content_for_deepseek_v4() {
        // deepseek-v4's official API was verified live NOT to require
        // reasoning_content on assistant tool-call messages (multi-round returns
        // 200 without it), and a "." stub could break non-official endpoints
        // (nvidia/vllm). So no stub — only kimi-k2 gets one. Real
        // reasoning_content the model returns is still round-tripped.
        let p = OpenAIProvider::new("key", "deepseek-v4-pro");
        let mut assistant = msg("the answer");
        assistant.role = MessageRole::Assistant;
        let msgs = [assistant];
        let v = serde_json::to_value(p.build_request(&msgs, &[], &ChatConfig::default(), false))
            .unwrap();
        let a = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message present");
        assert!(
            a.get("reasoning_content").is_none(),
            "deepseek-v4 assistant message must not get a reasoning_content stub"
        );
    }

    #[test]
    fn test_with_hints_overrides_detection() {
        let p = OpenAIProvider::new("key", "gpt-4o").with_hints(ModelHints {
            uses_completion_tokens: true,
            fixed_temperature: true,
            lacks_vision: true,
            merge_system_messages: false,
            reasoning_style: ReasoningStyle::None,
        });
        assert!(p.hints.uses_completion_tokens);
        assert!(p.hints.fixed_temperature);
        assert!(p.hints.lacks_vision);
        assert!(!p.hints.merge_system_messages);
    }

    #[test]
    fn test_build_content_strips_images_on_assistant_messages() {
        // Regression for live mini3 dspfac slides session
        // 1779130130502-th18yr: send_file(slide-NN.png) populated
        // assistant_msg.media in session_actor.rs, and on every
        // subsequent turn the openai provider re-encoded the same
        // generated PNGs into image_url content. That broke kimi
        // (400 InvalidParameter: "incorrect modal image") and wasted
        // ~1 MB per slide per call on vision-capable models.
        //
        // Inlining vision content should only flow from user→model,
        // never assistant→model on echo of its own tool outputs.
        let hints = ModelHints::default(); // lacks_vision: false
        let mut assistant = msg("I delivered the deck.");
        assistant.role = MessageRole::Assistant;
        assistant.media = vec!["skill-output/slides/deck/output/slide-01.png".to_string()];
        let content = build_openai_content(&assistant, &hints)
            .expect("assistant content should still be built");
        match content {
            OpenAIContent::Text(text) => {
                assert_eq!(text, "I delivered the deck.");
            }
            OpenAIContent::Parts(_) => {
                panic!("assistant message media must not produce image_url parts");
            }
        }
    }

    #[test]
    fn test_provider_metadata_uses_custom_endpoint_label() {
        let provider = OpenAIProvider::new("key", "kimi-k2.5")
            .with_provider_label("moonshot")
            .with_base_url("https://www.autodl.art/api/v1");

        let metadata = provider.provider_metadata();
        assert_eq!(metadata.provider, "moonshot");
        assert_eq!(metadata.model, "kimi-k2.5");
        assert_eq!(metadata.endpoint.as_deref(), Some("autodl.art"));
        assert_eq!(metadata.display_label(), "moonshot/kimi-k2.5 @ autodl.art");
    }

    // ── FailFast image-modality retry guard ───────────────────────────────────

    /// Build a User message that carries a `.png` image attachment so that
    /// `request_has_user_images` returns `true` (the path must end in a
    /// recognised image extension — checked by `vision::is_image`).
    fn msg_with_user_image() -> Message {
        Message {
            role: MessageRole::User,
            content: "look at this image".to_string(),
            media: vec!["/tmp/test_image.png".to_string()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }
    }

    /// The body string that `is_image_modality_error` recognises as an image-
    /// modality 400 (matches the `"does not support image"` arm).
    const IMAGE_MODALITY_400_BODY: &str = r#"{"error":{"message":"This model does not support image input","type":"invalid_request_error"}}"#;

    #[tokio::test]
    async fn should_not_retry_text_only_when_failfast_on_image_modality_400_stream() {
        use crate::{LlmCallPolicy, with_llm_call_policy};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(IMAGE_MODALITY_400_BODY)
                    .append_header("Content-Type", "application/json"),
            )
            .mount(&server)
            .await;

        let provider = OpenAIProvider::new("test-key", "gpt-4o").with_base_url(server.uri());
        let messages = vec![msg_with_user_image()];

        let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
            provider
                .chat_stream(&messages, &[], &ChatConfig::default())
                .await
        })
        .await;

        assert!(result.is_err(), "expected Err on 400, got Ok");
        let reqs = server.received_requests().await.unwrap_or_default();
        assert_eq!(
            reqs.len(),
            1,
            "FailFast must skip text-only retry; got {} request(s)",
            reqs.len()
        );
    }

    #[tokio::test]
    async fn should_not_retry_text_only_when_failfast_on_image_modality_400_chat() {
        use crate::{LlmCallPolicy, with_llm_call_policy};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(IMAGE_MODALITY_400_BODY)
                    .append_header("Content-Type", "application/json"),
            )
            .mount(&server)
            .await;

        let provider = OpenAIProvider::new("test-key", "gpt-4o").with_base_url(server.uri());
        let messages = vec![msg_with_user_image()];

        let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
            provider.chat(&messages, &[], &ChatConfig::default()).await
        })
        .await;

        assert!(result.is_err(), "expected Err on 400, got Ok");
        let reqs = server.received_requests().await.unwrap_or_default();
        assert_eq!(
            reqs.len(),
            1,
            "FailFast must skip text-only retry; got {} request(s)",
            reqs.len()
        );
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
