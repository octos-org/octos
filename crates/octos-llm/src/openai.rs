//! OpenAI (GPT) provider implementation.

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::{Message, MessageRole};

use reqwest::Client;
use serde::{Deserialize, Serialize};

use secrecy::{ExposeSecret, SecretString};

use crate::vision;

use crate::cache_manifest::{
    PromptCacheInputManifest, prompt_cache_features_enabled, without_cache_markers,
};
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
    /// Translates `ChatConfig::reasoning_effort` into request fields. The
    /// default style ignores enabled effort, while an explicit disabled effort
    /// uses the generic OpenAI-compatible control.
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

        // kimi-k3 pins its sampling params server-side (temperature=1.0,
        // top_p=0.95, …) and rejects overrides, so never send temperature. The
        // Kimi *coding plan* (family `moonshot-coding`) exposes the SAME K3
        // model under the bare ids `k3` / `kimi-for-coding*`, which don't
        // contain `kimi-k3` — match them too, or the endpoint 400s with
        // "invalid temperature: only 1 is allowed for this model".
        let fixed_temperature = is_o_series
            || m.starts_with("gpt-5")
            || m.contains("kimi-k2")
            || m.contains("kimi-k3")
            || m == "k3"
            // Kimi Code context-window variants (`k3-256k`) share the
            // temperature restriction; dash-prefix only, so an unrelated
            // "k30"-style name never matches.
            || m.starts_with("k3-")
            || m.starts_with("kimi-for-coding")
            || m == "gpt-4.1-nano";

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
        // grok-4.x take a plain `reasoning_effort`; Kimi K3 takes a top-level
        // `reasoning_effort` whose only accepted value is `"max"` (thinking is
        // always on and K3 rejects the K2.x `thinking` object). Everything else
        // emits nothing.
        // Effort/thinking is only EMITTED when an operator sets `reasoning_effort`
        // (opt-in), and the style is config-overridable per route — important
        // because the same `deepseek-v4` name fronts endpoints that differ
        // (api.deepseek.com accepts it; nvidia/vllm may not), same caveat as
        // `lacks_vision`. `grok` is narrowed to `grok-4` since older Grok
        // families can reject enabled `reasoning_effort` values. An operator
        // may still explicitly request `none` for a compatible endpoint.
        let reasoning_style = if m.contains("deepseek-v4") || m.contains("deepseek-reasoner") {
            ReasoningStyle::EffortAndThinkingToggle
        } else if m.contains("kimi-k3")
            || m == "k3"
            || m.starts_with("k3-")
            || m.starts_with("kimi-for-coding")
        {
            // K3, incl. the coding plan's bare `k3` / `k3-256k` and
            // `kimi-for-coding*` ids (same K3 model, different ids that don't
            // contain `kimi-k3`): per its quickstart docs `reasoning_effort`
            // accepts low|high|max (default max); thinking is always on and
            // the K2.x `thinking` object is rejected. Graded effort IS
            // honored — do NOT collapse everything to "max". (These ids
            // already pin temperature above; they must get the graded style
            // too or `/thinking` is a no-op.)
            ReasoningStyle::EffortLowHighMax
        } else if m.contains("glm-4.5")
            || m.contains("glm-4.6")
            || m.contains("glm-5")
            || m.contains("glm-z")
        {
            // GLM-4.5+/4.6/5.x + the z-reasoning line (Zhipu / Z.ai, e.g.
            // `glm-5.2`): thinking is a binary `thinking:{"type":"enabled"}`
            // toggle, no graded effort. Any set effort level enables thinking.
            // Narrowed from a bare `contains("glm")`: legacy `glm-4`/`glm-4-plus`/
            // `glm-3` REJECT the thinking object (400), so they must not match.
            // NOTE: the SHIPPED `glm-5.2` route runs through AnthropicProvider
            // (Z.ai's Anthropic-compatible endpoint), which maps `/thinking` via
            // `build_anthropic_thinking`; this arm only governs a GLM added
            // through an OpenAI-compatible endpoint.
            ReasoningStyle::ThinkingToggle
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
    /// No enabled reasoning control emitted on the chat path. Explicitly disabled
    /// reasoning uses the generic `reasoning_effort: "none"` control.
    #[default]
    None,
    /// Top-level `reasoning_effort: "low"|"medium"|"high"` — OpenAI chat-completions
    /// reasoning models (o-series / gpt-5) and xAI Grok.
    Effort,
    /// `reasoning_effort` plus `thinking: {"type": "enabled"}` — DeepSeek V4.
    EffortAndThinkingToggle,
    /// Top-level `reasoning_effort` whose only accepted value is `"max"`. Legacy
    /// / manual-override only — retained for configs that pin it; Kimi K3 now uses
    /// [`ReasoningStyle::EffortLowHighMax`] since K3's docs list `low|high|max`.
    EffortMaxOnly,
    /// Top-level `reasoning_effort: "low"|"high"|"max"` (default `"max"`) — Kimi
    /// K3. Per K3's quickstart docs it accepts exactly those three values, thinking
    /// is ALWAYS on, and the K2.x `thinking` object must NOT be sent. octos has no
    /// K3-native "medium" tier, so `Medium` clamps up to `"high"`; `Max` maps to
    /// `"max"` (NOT clamped to "high" like the Effort style). No effort configured
    /// ⇒ nothing emitted (K3 still thinks — its server-side `max` default).
    EffortLowHighMax,
    /// Binary `thinking: {"type": "enabled"}` toggle with NO `reasoning_effort` —
    /// GLM-4.5+/5.x (Zhipu / Z.ai), which control thinking via enable/disable and
    /// do not accept graded effort. Any configured effort level ENABLES thinking;
    /// no effort configured ⇒ nothing emitted (the model's server-side default).
    ThinkingToggle,
}

/// Serialize tool-call arguments for the request wire as a JSON **object**
/// string. Chat/completions providers validate `function.arguments` and reject
/// the ENTIRE request with a non-retryable HTTP 400 when it does not decode to
/// an object — which a tool call recovered from inline/malformed model output
/// can be (a bare string, or a truncated fragment). Coercing a non-object to
/// `{}` keeps the request valid; the model then sees an ordinary empty-arg call
/// and can retry, instead of the whole turn/task dying. Objects pass through
/// byte-identically, so this is a no-op on the normal path. See #1711.
fn tool_call_arguments_to_wire(arguments: &serde_json::Value) -> String {
    if arguments.is_object() {
        return arguments.to_string();
    }
    // Recover a stringified object (e.g. `"{\"command\":\"ls\"}"`).
    if let serde_json::Value::String(inner) = arguments
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(inner)
        && parsed.is_object()
    {
        return parsed.to_string();
    }
    tracing::warn!(
        target: "octos::toolcall_repair",
        "coerced non-object tool-call arguments to empty object at request boundary (#1711)"
    );
    "{}".to_string()
}

/// OpenAI GPT provider.
pub struct OpenAIProvider {
    client: Client,
    /// Separate client for streaming requests, built without a total request
    /// timeout so a healthy long generation is never cut off mid-stream. See
    /// [`crate::provider::build_streaming_http_client`].
    stream_client: Client,
    api_key: SecretString,
    model: String,
    base_url: String,
    hints: ModelHints,
    /// Label for logs/failover. Defaults to `"openai"` but overridden by
    /// registry entries (e.g. `"moonshot"`, `"deepseek"`) so providers are
    /// distinguishable in failover chains.
    provider_label: String,
    /// OpenAI-only request affinity. Defaults to official-endpoint-only so
    /// Kimi/DeepSeek/vLLM never see a reserved field they may reject; the
    /// operator kill-switch (`OCTOS_PROMPT_CACHING`) is evaluated per request.
    prompt_cache_affinity: bool,
    /// Whether a builder call explicitly selected the affinity mode. An
    /// explicit opt-in/out must survive either builder-call order (mirrors
    /// `AnthropicProvider::prompt_caching_override`).
    prompt_cache_affinity_override: Option<bool>,
}

const OFFICIAL_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// Official-endpoint check tolerant of a trailing slash, surrounding
/// whitespace, and case, so `https://api.openai.com/v1/` keeps affinity and
/// is not tagged as a custom host.
fn is_official_openai_base_url(base_url: &str) -> bool {
    base_url
        .trim()
        .trim_end_matches('/')
        .eq_ignore_ascii_case(OFFICIAL_OPENAI_BASE_URL)
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
            stream_client: crate::provider::build_streaming_http_client(
                crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            ),
            api_key: SecretString::from(api_key.into()),
            hints,
            model,
            base_url: OFFICIAL_OPENAI_BASE_URL.to_string(),
            provider_label: "openai".to_string(),
            prompt_cache_affinity: true,
            prompt_cache_affinity_override: None,
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
        let official = is_official_openai_base_url(&url);
        // Affinity is official-endpoint-only by default; an explicit builder
        // choice survives either call order.
        if self.prompt_cache_affinity_override.is_none() {
            self.prompt_cache_affinity = official;
        }
        // If using a non-default base URL, tag the provider_label to distinguish
        // it in the adaptive router (e.g., "moonshot@autodl" vs "moonshot").
        if !official
            && let Some(domain) = url
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

    /// Explicit override for an endpoint known to implement OpenAI's
    /// `prompt_cache_key` contract. Custom endpoints remain opt-out by
    /// default; this explicit choice survives either builder-call order. The
    /// operator kill-switch (`OCTOS_PROMPT_CACHING`) still applies per request.
    pub fn with_prompt_cache_affinity(mut self, enabled: bool) -> Self {
        self.prompt_cache_affinity = enabled;
        self.prompt_cache_affinity_override = Some(enabled);
        self
    }

    /// Affinity key for this request, if any. `features_enabled` is the
    /// operator kill-switch (`OCTOS_PROMPT_CACHING`), passed in so the
    /// decision is made per request — like the Responses provider — and stays
    /// unit-testable without mutating process env.
    fn prompt_cache_key_for<'a>(
        &self,
        config: &'a ChatConfig,
        features_enabled: bool,
    ) -> Option<&'a str> {
        if !(self.prompt_cache_affinity && features_enabled) {
            return None;
        }
        config
            .prompt_cache_context
            .as_ref()
            .map(|context| context.affinity_key.as_str())
    }

    /// Override the auto-detected model hints.
    pub fn with_hints(mut self, hints: ModelHints) -> Self {
        self.hints = hints;
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
            .wrap_err_with(|| {
                crate::provider::transport_error_message(
                    false,
                    &self.provider_label,
                    &self.model,
                    crate::provider::ApiStyle::OpenAiChatCompletions,
                )
            })
    }

    /// POST a streaming chat request (adds `stream` + `stream_options`).
    /// Factored for the same image-modality fallback as [`Self::post_chat`].
    async fn post_chat_stream(&self, request: &OpenAIRequest<'_>) -> Result<reqwest::Response> {
        let mut body = serde_json::to_value(request).wrap_err_with(|| {
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
        // Stream client: no total timeout, so a long healthy generation is not
        // cut off. Stalls are bounded by the client's per-read timeout and the
        // agent's stream-timeout guards (see build_streaming_http_client).
        self.stream_client
            .post(format!("{}/chat/completions", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err_with(|| {
                crate::provider::transport_error_message(
                    true,
                    &self.provider_label,
                    &self.model,
                    crate::provider::ApiStyle::OpenAiChatCompletions,
                )
            })
    }

    /// Lane-attributed wording for operational failures (see
    /// [`crate::provider::operational_error_message`]).
    fn operational_message(&self, stage: crate::provider::OperationalStage) -> String {
        crate::provider::operational_error_message(
            stage,
            &self.provider_label,
            &self.model,
            crate::provider::ApiStyle::OpenAiChatCompletions,
        )
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
        config: &'a ChatConfig,
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
                                arguments: tool_call_arguments_to_wire(&tc.arguments),
                            },
                        })
                        .collect()
                });
                // We do NOT re-send prior assistant reasoning_content for ordinary
                // openai-compat models. Reasoning models re-derive their chain of
                // thought each turn, so round-tripping the full verbose reasoning is
                // pure context bloat (and grows unboundedly across a tool loop) —
                // OpenAI's own API and codex both drop it.
                //
                // kimi-k2/k3 are the exception. With thinking enabled kimi-k2 (a)
                // returns 400 "reasoning_content is missing in assistant tool call
                // message" if the field is absent, AND (b) per kimi's docs preserves
                // historical assistant reasoning for multi-step tool-use continuity
                // (K3's quickstart likewise mandates "add the complete assistant
                // message returned by the API to the next request. Do not keep only
                // `content`"). So for kimi-k2/k3 we keep the REAL reasoning when
                // present, and fall back to a minimal "." stub only to satisfy the
                // presence check when it's absent.
                //
                // kimi-k2/k3 are detected via fixed_temperature + model name
                // containing "kimi-k2"/"kimi-k3". Other models (e.g. deepseek-v4,
                // verified live to return 200 without the field, and non-official
                // nvidia/vllm endpoints that don't expect it) get no
                // reasoning_content at all.
                let model_lower = self.model.to_lowercase();
                let needs_reasoning_stub = self.hints.fixed_temperature
                    && (model_lower.contains("kimi-k2")
                        || model_lower.contains("kimi-k3")
                        // Kimi Code API ids: bare `k3`/`k3-256k` and the
                        // K2.7 Code alias — thinking is always on for them,
                        // so assistant tool-call messages need the stub too.
                        || model_lower == "k3"
                        || model_lower.starts_with("k3-")
                        || model_lower.starts_with("kimi-for-coding"));
                let reasoning = if role == "assistant" && needs_reasoning_stub {
                    match m.reasoning_content.as_deref() {
                        Some(r) if !r.is_empty() => Some(r),
                        _ => Some("."),
                    }
                } else {
                    None
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
        // request fields the model's ReasoningStyle expects. Enabled effort is
        // emitted only for a declared style. Explicitly disabled reasoning is
        // also forwarded to generic OpenAI-compatible endpoints because local
        // servers such as Ollama accept the standard `reasoning_effort: "none"`.
        let (reasoning_effort, thinking): (Option<&str>, Option<serde_json::Value>) =
            match (config.reasoning_effort, self.hints.reasoning_style) {
                (Some(effort), style) => {
                    use crate::config::ReasoningEffort as RE;
                    let enabled = || serde_json::json!({ "type": "enabled" });
                    let disabled = || serde_json::json!({ "type": "disabled" });
                    match (effort, style) {
                        // Toggle-based providers disable thinking through their
                        // native binary control and reject reasoning_effort.
                        (
                            RE::Disabled,
                            ReasoningStyle::ThinkingToggle
                            | ReasoningStyle::EffortAndThinkingToggle,
                        ) => (None, Some(disabled())),
                        // Kimi K3 has no documented off value. Omitting the
                        // control preserves the provider default.
                        (
                            RE::Disabled,
                            ReasoningStyle::EffortLowHighMax | ReasoningStyle::EffortMaxOnly,
                        ) => (None, None),
                        // Standard and generic OpenAI-compatible endpoints use
                        // reasoning_effort:"none" to disable reasoning.
                        (RE::Disabled, ReasoningStyle::Effort | ReasoningStyle::None) => {
                            (Some("none"), None)
                        }
                        // GLM-4.5+/5.x: binary thinking toggle, NO reasoning_effort.
                        (_, ReasoningStyle::ThinkingToggle) => (None, Some(enabled())),
                        // Kimi K3: low|high|max (no medium tier → clamp up to high;
                        // Max stays "max", NOT clamped to "high"). Thinking always on.
                        (_, ReasoningStyle::EffortLowHighMax) => {
                            let e = match effort {
                                RE::Disabled => unreachable!(),
                                RE::Low => "low",
                                RE::Medium | RE::High => "high",
                                RE::Max => "max",
                            };
                            (Some(e), None)
                        }
                        // Legacy manual-override: everything → "max".
                        (_, ReasoningStyle::EffortMaxOnly) => (Some("max"), None),
                        // DeepSeek V4: reasoning_effort + `thinking` toggle.
                        (_, ReasoningStyle::EffortAndThinkingToggle) => {
                            let e = match effort {
                                RE::Disabled => unreachable!(),
                                RE::Low => "low",
                                RE::Medium => "medium",
                                RE::High => "high",
                                RE::Max => "max",
                            };
                            (Some(e), Some(enabled()))
                        }
                        // OpenAI/Grok: low|medium|high, no max tier → clamp to high.
                        (_, ReasoningStyle::Effort) => {
                            let e = match effort {
                                RE::Disabled => unreachable!(),
                                RE::Low => "low",
                                RE::Medium => "medium",
                                RE::High => "high",
                                RE::Max => "high",
                            };
                            (Some(e), None)
                        }
                        (_, ReasoningStyle::None) => (None, None),
                    }
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
            // Flatten operator-supplied sampler params (e.g. repeat_penalty) into
            // the request. Empty unless configured → no wire change for cloud.
            extra_sampling: {
                let mut extra = config.sampling_params.clone().unwrap_or_default();
                // Defense-in-depth (#2172): drop keys octos already models with
                // dedicated fields, so a misconfigured sampler param can't emit
                // duplicate/divergent keys across the streaming (to_value,
                // last-wins) and non-streaming (to_vec, duplicate) send paths.
                // The strip must not be silent (#2177): an operator who puts
                // e.g. `temperature` in sampling_params would otherwise see a
                // configured-but-unchanged request with no signal at any level.
                let mut dropped = Vec::new();
                for reserved in RESERVED_SAMPLING_KEYS {
                    if extra.remove(*reserved).is_some() {
                        dropped.push(*reserved);
                    }
                }
                if !dropped.is_empty() {
                    tracing::warn!(
                        provider = %self.provider_label,
                        model = %self.model,
                        dropped = ?dropped,
                        "sampling_params keys collide with reserved request fields and are dropped; set them via the dedicated ChatConfig field where one exists (e.g. temperature, max_tokens) — keys without one must not be sent"
                    );
                }
                extra
            },
            prompt_cache_key: self.prompt_cache_key_for(config, prompt_cache_features_enabled()),
            tool_choice: config.tool_choice.openai_chat_wire(!tools.is_empty()),
        }
    }

    fn prompt_cache_input_manifest(
        &self,
        request: &OpenAIRequest<'_>,
        config: &ChatConfig,
    ) -> PromptCacheInputManifest {
        let normalized = without_cache_markers(
            serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
        );
        let mut stable = Vec::new();
        let mut conversation = Vec::new();
        if let Some(messages) = normalized
            .get("messages")
            .and_then(|value| value.as_array())
        {
            for (index, message) in messages.iter().enumerate() {
                let role = message
                    .get("role")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown");
                let segment = (format!("message:{index}:{role}"), message.clone());
                if role == "system" || role == "developer" {
                    stable.push(segment);
                } else {
                    conversation.push(segment);
                }
            }
        }
        if let Some(tools) = normalized.get("tools").and_then(|value| value.as_array()) {
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

    fn trace_prompt_cache_input(&self, request: &OpenAIRequest<'_>, config: &ChatConfig) {
        if tracing::enabled!(target: "octos.prompt_cache", tracing::Level::TRACE) {
            self.prompt_cache_input_manifest(request, config).trace();
        }
    }
}

/// Request keys octos sets via dedicated `OpenAIRequest` fields; if an operator
/// puts one of these in `sampling_params` it is dropped (the dedicated field /
/// knob wins) rather than emitted twice. See [`OpenAIProvider::build_request`].
const RESERVED_SAMPLING_KEYS: &[&str] = &[
    "model",
    "messages",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "tools",
    "response_format",
    "reasoning_effort",
    "thinking",
    "stream",
    "stream_options",
    "prompt_cache_key",
    "tool_choice",
];

#[async_trait]
impl LlmProvider for OpenAIProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let request = self.build_request(messages, tools, config, false);
        self.trace_prompt_cache_input(&request, config);
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
                self.trace_prompt_cache_input(&retry, config);
                response = self.post_chat(&retry).await?;
            } else {
                let body = crate::provider::truncate_error_body(&body);
                return Err(crate::error::LlmError::from_status_with_label(
                    400,
                    &body,
                    format!("{}/{}", self.provider_label, self.model),
                )
                .with_api_style(crate::provider::ApiStyle::OpenAiChatCompletions)
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
            .with_api_style(crate::provider::ApiStyle::OpenAiChatCompletions)
            .into());
        }

        let api_response: OpenAIResponse = response.json().await.wrap_err_with(|| {
            self.operational_message(crate::provider::OperationalStage::ParseResponse)
        })?;

        let choice = api_response.choices.into_iter().next().ok_or_else(|| {
            eyre::Report::msg(
                self.operational_message(crate::provider::OperationalStage::NoChoices),
            )
        })?;

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
            usage: {
                // OpenAI reports cached tokens INSIDE prompt_tokens; the
                // TokenUsage contract is disjoint (Anthropic-style: total
                // prompt = input + cache_read), so subtract at the boundary.
                let cached = api_response
                    .usage
                    .prompt_tokens_details
                    .as_ref()
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0);
                let cache_write = api_response
                    .usage
                    .prompt_tokens_details
                    .as_ref()
                    .map(|d| d.cache_write_tokens)
                    .unwrap_or(0);
                TokenUsage {
                    input_tokens: api_response
                        .usage
                        .prompt_tokens
                        .saturating_sub(cached)
                        .saturating_sub(cache_write),
                    output_tokens: api_response.usage.completion_tokens,
                    reasoning_tokens: api_response
                        .usage
                        .completion_tokens_details
                        .as_ref()
                        .map(|details| details.reasoning_tokens)
                        .unwrap_or(0),
                    cache_read_tokens: cached,
                    cache_write_tokens: cache_write,
                    ..Default::default()
                }
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
        self.trace_prompt_cache_input(&request, config);
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
                self.trace_prompt_cache_input(&retry, config);
                response = self.post_chat_stream(&retry).await?;
            } else {
                let body = crate::provider::truncate_error_body(&text);
                return Err(crate::error::LlmError::from_status_with_label(
                    400,
                    &body,
                    format!("{}/{}", self.provider_label, self.model),
                )
                .with_api_style(crate::provider::ApiStyle::OpenAiChatCompletions)
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
            .with_api_style(crate::provider::ApiStyle::OpenAiChatCompletions)
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

    fn api_style(&self) -> Option<crate::provider::ApiStyle> {
        Some(crate::provider::ApiStyle::OpenAiChatCompletions)
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
    /// Operator-supplied extra sampler params flattened into the request body
    /// (`repeat_penalty`, `top_p`, `top_k`, `min_p`, `frequency_penalty`, …) for
    /// OpenAI-compatible servers (llama.cpp / vLLM / SGLang) — params octos does
    /// not model. Empty by default, so it flattens to nothing and cloud requests
    /// are unchanged. See `ChatConfig::sampling_params` / issue #2172.
    #[serde(flatten)]
    extra_sampling: serde_json::Map<String, serde_json::Value>,
    /// Official OpenAI affinity key. Omitted for every compatible/custom
    /// endpoint unless the provider capability is explicitly enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    /// `ChatConfig.tool_choice` on the wire; absent for the default `auto`
    /// so ordinary requests keep their exact prior shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
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
    /// Automatic prompt-cache breakdown. `cached_tokens` counts the portion
    /// of `prompt_tokens` served from OpenAI's cache (INCLUDED in
    /// `prompt_tokens`, unlike Anthropic's disjoint accounting). Compat
    /// providers that omit the object parse as `None`.
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// Reasoning is already included in completion_tokens; retain this
    /// diagnostic breakdown without adding it to the billed output total.
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Deserialize, Default)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

#[derive(Deserialize, Default)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
    #[serde(default)]
    cache_write_tokens: u32,
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
        // OpenAI reports cached tokens INSIDE prompt_tokens; the TokenUsage
        // contract is disjoint (total prompt = input + cache_read).
        let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        let cached = usage["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0) as u32;
        let cache_write = usage["prompt_tokens_details"]["cache_write_tokens"]
            .as_u64()
            .unwrap_or(0) as u32;
        events.push(StreamEvent::Usage(TokenUsage {
            input_tokens: prompt.saturating_sub(cached).saturating_sub(cache_write),
            output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0) as u32,
            reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .unwrap_or(0) as u32,
            cache_read_tokens: cached,
            cache_write_tokens: cache_write,
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

    #[test]
    fn provider_normalized_manifest_proves_same_epoch_append_only_prefix() {
        let provider = OpenAIProvider::new("test-key", "gpt-5.4");
        let config = ChatConfig {
            prompt_cache_context: Some(crate::PromptCacheContext {
                affinity_key: "octos-affinity".to_owned(),
                epoch_id: "epoch-one".to_owned(),
                stable_prefix_hash: "agent-stable".to_owned(),
                semantic_boundaries: Vec::new(),
            }),
            ..Default::default()
        };
        let tools = vec![ToolSpec {
            name: "read".to_owned(),
            description: "read a file".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let first_messages = vec![
            Message::system("TOPSECRET_SYSTEM_3892"),
            Message::user("TOPSECRET_USER_1047"),
        ];
        let mut next_messages = first_messages.clone();
        next_messages.push(Message::assistant("answer"));
        next_messages.push(Message::user("next"));

        let first_request = provider.build_request(&first_messages, &tools, &config, false);
        let next_request = provider.build_request(&next_messages, &tools, &config, false);
        let first = provider.prompt_cache_input_manifest(&first_request, &config);
        let next = provider.prompt_cache_input_manifest(&next_request, &config);
        let comparison = first.compare_prefix(&next);

        assert_eq!(first.epoch_id.as_deref(), Some("epoch-one"));
        assert_eq!(first.stable_prefix_hash, next.stable_prefix_hash);
        assert_eq!(comparison.conversation_prefix_segments, 1);
        assert_eq!(comparison.invalidation_reason, None);
        assert!(comparison.reusable_normalized_bytes > 0);
        let redacted = serde_json::to_string(&first).unwrap();
        assert!(!redacted.contains("TOPSECRET_SYSTEM_3892"));
        assert!(!redacted.contains("TOPSECRET_USER_1047"));
    }

    /// A custom base URL tags the router label (`moonshot-coding@api`), but
    /// `provider_metadata()` reports the untagged lane. The manifest must use
    /// the metadata label, otherwise usage rows (attributed through
    /// `provider_metadata_for_index`) never match their manifest and the OUP
    /// epoch reads a route change on every call.
    #[test]
    fn should_build_manifest_with_the_same_provider_label_as_provider_metadata_for_tagged_lane() {
        let provider = OpenAIProvider::new("test-key", "k3")
            .with_provider_label("moonshot-coding")
            .with_base_url("https://api.kimi.com/coding/v1");
        assert_eq!(provider.provider_name(), "moonshot-coding@api");
        let config = ChatConfig::default();
        let messages = vec![Message::system("stable"), Message::user("hello")];
        let request = provider.build_request(&messages, &[], &config, false);
        let manifest = provider.prompt_cache_input_manifest(&request, &config);

        let metadata = provider.provider_metadata();
        assert_eq!(metadata.provider, "moonshot-coding");
        assert_eq!(manifest.provider, metadata.provider);
        assert_eq!(manifest.model, metadata.model);
        assert_eq!(
            manifest.provider,
            provider.provider_metadata_for_index(None).provider
        );
    }

    /// `ChatConfig.tool_choice` used to be inert: no adapter serialized it,
    /// so a "tools-disabled" round (the convergence reflection) still let
    /// the model call tools. `none` must reach the wire, while the default
    /// `auto` and tool-less requests keep their exact prior body.
    #[test]
    fn should_serialize_tool_choice_on_the_wire_only_when_explicit() {
        let provider = OpenAIProvider::new("test-key", "gpt-5.4");
        let tools = vec![ToolSpec {
            name: "read".to_owned(),
            description: "read a file".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let messages = vec![Message::user("hello")];
        let auto = serde_json::to_value(provider.build_request(
            &messages,
            &tools,
            &ChatConfig::default(),
            false,
        ))
        .unwrap();
        assert!(auto.get("tool_choice").is_none(), "{auto}");

        let none = ChatConfig {
            tool_choice: crate::ToolChoice::None,
            ..Default::default()
        };
        let request =
            serde_json::to_value(provider.build_request(&messages, &tools, &none, false)).unwrap();
        assert_eq!(request["tool_choice"], "none");
        let tool_less =
            serde_json::to_value(provider.build_request(&messages, &[], &none, false)).unwrap();
        assert!(tool_less.get("tool_choice").is_none(), "{tool_less}");
    }

    #[test]
    fn tool_call_arguments_wire_passes_objects_through() {
        let obj = serde_json::json!({"command": "ls -la"});
        let wire = tool_call_arguments_to_wire(&obj);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&wire).unwrap(),
            obj
        );
    }

    #[test]
    fn tool_call_arguments_wire_coerces_non_object_to_empty_object() {
        // A bare string is what the old inline fallback produced; serialized
        // verbatim it caused the provider HTTP 400. It must become `{}`.
        let bare = serde_json::Value::String("git clone https://x".to_string());
        assert_eq!(tool_call_arguments_to_wire(&bare), "{}");
        // Arrays/numbers/null are also not valid arguments objects.
        assert_eq!(
            tool_call_arguments_to_wire(&serde_json::json!([1, 2])),
            "{}"
        );
        assert_eq!(tool_call_arguments_to_wire(&serde_json::Value::Null), "{}");
    }

    #[test]
    fn tool_call_arguments_wire_recovers_a_stringified_object() {
        let stringified = serde_json::Value::String(r#"{"command":"ls"}"#.to_string());
        let wire = tool_call_arguments_to_wire(&stringified);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&wire).unwrap(),
            serde_json::json!({"command": "ls"})
        );
    }

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
        // Kimi K3 (incl. provider-prefixed) -> low|high|max reasoning_effort.
        assert_eq!(
            ModelHints::detect("kimi-k3").reasoning_style,
            ReasoningStyle::EffortLowHighMax
        );
        assert_eq!(
            ModelHints::detect("moonshotai/kimi-k3").reasoning_style,
            ReasoningStyle::EffortLowHighMax
        );
        // GLM-4.5+/5.x (Zhipu / Z.ai) -> binary thinking toggle.
        assert_eq!(
            ModelHints::detect("glm-5.2").reasoning_style,
            ReasoningStyle::ThinkingToggle
        );
        assert_eq!(
            ModelHints::detect("zai-org/glm-4.6").reasoning_style,
            ReasoningStyle::ThinkingToggle
        );
        // Legacy GLM (pre-4.5) REJECTS the `thinking` object — it must NOT get
        // the toggle style (a bare `contains("glm")` used to misfire here and
        // send an unsupported field → 400).
        for legacy in ["glm-4", "glm-4-plus", "zhipu/glm-4-air", "glm-3-turbo"] {
            assert_eq!(
                ModelHints::detect(legacy).reasoning_style,
                ReasoningStyle::None,
                "{legacy} predates the thinking toggle and must emit no reasoning control"
            );
        }
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
    fn prompt_cache_key_is_capability_gated_to_official_openai_endpoint() {
        let config = ChatConfig {
            prompt_cache_context: Some(crate::PromptCacheContext {
                affinity_key: "octos-stable-affinity".to_owned(),
                epoch_id: "epoch".to_owned(),
                stable_prefix_hash: "sha256:stable".to_owned(),
                semantic_boundaries: Vec::new(),
            }),
            ..Default::default()
        };
        let messages = [msg("hello")];
        let official = OpenAIProvider::new("key", "gpt-5").with_prompt_cache_affinity(true);
        let official_body =
            serde_json::to_value(official.build_request(&messages, &[], &config, false)).unwrap();
        assert_eq!(official_body["prompt_cache_key"], "octos-stable-affinity");

        for custom in [
            OpenAIProvider::new("key", "kimi-k3").with_base_url("https://api.moonshot.ai/v1"),
            OpenAIProvider::new("key", "deepseek-v4").with_base_url("https://api.deepseek.com/v1"),
        ] {
            let body =
                serde_json::to_value(custom.build_request(&messages, &[], &config, false)).unwrap();
            assert!(
                body.get("prompt_cache_key").is_none(),
                "compatible endpoints must not receive reserved OpenAI fields: {body}"
            );
        }
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
    fn build_request_flattens_sampling_params() {
        // Operator-supplied sampler params (#2172) appear as top-level fields in
        // the request body, so an OpenAI-compatible server receives e.g.
        // repeat_penalty even though octos does not model it.
        let p = OpenAIProvider::new("key", "gpt-4o");
        let mut sp = serde_json::Map::new();
        sp.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
        sp.insert("top_p".to_string(), serde_json::json!(0.95));
        let cfg = ChatConfig {
            sampling_params: Some(sp),
            ..Default::default()
        };
        let msgs = [msg("hi")];
        let v = serde_json::to_value(p.build_request(&msgs, &[], &cfg, false)).unwrap();
        assert_eq!(v["repeat_penalty"], serde_json::json!(1.1));
        assert_eq!(v["top_p"], serde_json::json!(0.95));
    }

    #[test]
    fn build_request_drops_reserved_keys_from_sampling_params() {
        // Defense-in-depth (#2172): a modeled key put in sampling_params is
        // dropped so it can't duplicate/override the dedicated field. The
        // dedicated `temperature` (0.5, exactly representable) wins; the stray
        // one (1.9) is gone. `repeat_penalty` (unmodeled) passes through.
        let p = OpenAIProvider::new("key", "gpt-4o");
        let mut sp = serde_json::Map::new();
        sp.insert("temperature".to_string(), serde_json::json!(1.9));
        sp.insert(
            "prompt_cache_key".to_string(),
            serde_json::json!("injected"),
        );
        sp.insert("tool_choice".to_string(), serde_json::json!("required"));
        sp.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
        let cfg = ChatConfig {
            temperature: Some(0.5),
            sampling_params: Some(sp),
            ..Default::default()
        };
        let msgs = [msg("hi")];
        let v = serde_json::to_value(p.build_request(&msgs, &[], &cfg, false)).unwrap();
        assert_eq!(v["temperature"], serde_json::json!(0.5));
        assert!(v.get("prompt_cache_key").is_none(), "{v}");
        assert!(v.get("tool_choice").is_none(), "{v}");
        assert_eq!(v["repeat_penalty"], serde_json::json!(1.1));
    }

    /// In-memory log capture so a test can assert a `tracing::warn!` fired
    /// (same shape as octos-cli's turn_trace tests).
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs(run: impl FnOnce()) -> String {
        let captured = CapturedLogs::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(captured.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn should_warn_when_sampling_params_contain_reserved_keys() {
        // #2177: stripping reserved keys from sampling_params must not be
        // silent — an operator who puts `temperature` there otherwise gets a
        // configured-but-unchanged request with zero signal.
        let p = OpenAIProvider::new("key", "gpt-4o");
        let mut sp = serde_json::Map::new();
        sp.insert("temperature".to_string(), serde_json::json!(1.9));
        sp.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
        let cfg = ChatConfig {
            sampling_params: Some(sp),
            ..Default::default()
        };
        let msgs = [msg("hi")];
        let logs = capture_logs(|| {
            p.build_request(&msgs, &[], &cfg, false);
        });
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("temperature"), "{logs}");
        assert!(
            !logs.contains("repeat_penalty"),
            "non-reserved keys pass through and are not named in the warning: {logs}"
        );
    }

    #[test]
    fn should_not_warn_when_sampling_params_have_no_reserved_keys() {
        let p = OpenAIProvider::new("key", "gpt-4o");
        let mut sp = serde_json::Map::new();
        sp.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
        let cfg = ChatConfig {
            sampling_params: Some(sp),
            ..Default::default()
        };
        let msgs = [msg("hi")];
        let logs = capture_logs(|| {
            p.build_request(&msgs, &[], &cfg, false);
        });
        assert!(
            !logs.contains("WARN"),
            "no reserved keys → no warning, got: {logs}"
        );
    }

    #[test]
    fn build_request_omits_sampling_params_when_unset() {
        // Cloud-safety: with no sampling_params, no extra keys are added — the
        // request body is unchanged.
        let p = OpenAIProvider::new("key", "gpt-4o");
        let msgs = [msg("hi")];
        let v = serde_json::to_value(p.build_request(&msgs, &[], &ChatConfig::default(), false))
            .unwrap();
        assert!(v.get("repeat_penalty").is_none());
        assert!(v.get("top_p").is_none());
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
    fn should_emit_none_when_reasoning_is_disabled_for_openai_compatible_endpoint() {
        let effort = serde_json::from_value(serde_json::json!("none"))
            .expect("none should disable reasoning");
        let provider =
            OpenAIProvider::new("key", "qwen3.5:9b").with_base_url("http://localhost:11434/v1");
        let config = ChatConfig {
            reasoning_effort: Some(effort),
            ..Default::default()
        };

        let request = serde_json::to_value(provider.build_request(
            &[msg("return JSON")],
            &[],
            &config,
            false,
        ))
        .unwrap();

        assert_eq!(request["reasoning_effort"], "none");
        assert!(request.get("thinking").is_none());
    }

    #[test]
    fn should_disable_thinking_toggle_when_reasoning_is_disabled() {
        let effort = serde_json::from_value(serde_json::json!("none"))
            .expect("none should disable reasoning");
        let provider = OpenAIProvider::new("key", "glm-5.2");
        let config = ChatConfig {
            reasoning_effort: Some(effort),
            ..Default::default()
        };

        let request = serde_json::to_value(provider.build_request(
            &[msg("return JSON")],
            &[],
            &config,
            false,
        ))
        .unwrap();

        assert_eq!(
            request["thinking"],
            serde_json::json!({ "type": "disabled" })
        );
        assert!(request.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_request_stubs_reasoning_content_for_bare_k3_ids() {
        // Kimi Code API model ids are the BARE `k3` / `k3-256k` /
        // `kimi-for-coding*` — the old gate only matched "kimi-k2"/"kimi-k3"
        // substrings, so the exact ids Kimi Code serves got NO stub and risked
        // 400 "reasoning_content is missing" on multi-round tool calls.
        for model in [
            "k3",
            "k3-256k",
            "kimi-for-coding",
            "kimi-for-coding-highspeed",
        ] {
            let p = OpenAIProvider::new("key", model);
            let mut assistant = msg("the answer");
            assistant.role = MessageRole::Assistant;
            let msgs = [assistant];
            let v =
                serde_json::to_value(p.build_request(&msgs, &[], &ChatConfig::default(), false))
                    .unwrap();
            let a = v["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "assistant")
                .expect("assistant message present");
            assert_eq!(
                a.get("reasoning_content").and_then(|r| r.as_str()),
                Some("."),
                "{model} assistant message must carry the reasoning stub"
            );
        }
    }

    #[test]
    fn build_request_does_not_stub_reasoning_content_for_deepseek_v4() {
        // deepseek-v4's official API was verified live NOT to require
        // reasoning_content on assistant tool-call messages (multi-round returns
        // 200 without it), and a "." stub could break non-official endpoints
        // (nvidia/vllm). So no stub — only kimi-k2 gets one.
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
    fn build_request_drops_prior_reasoning_content_for_non_kimi_model() {
        // (a) A non-kimi reasoning model must NOT have prior verbose
        // reasoning_content round-tripped back into the request — reasoning
        // models re-derive their chain of thought each turn, so re-sending it
        // is pure context bloat. The field must be absent entirely.
        let p = OpenAIProvider::new("key", "deepseek-v4-pro");
        let mut assistant = msg("the final answer");
        assistant.role = MessageRole::Assistant;
        assistant.reasoning_content =
            Some("a very long prior chain of thought that should not be re-sent".to_string());
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
            "non-kimi model must drop prior reasoning_content, got: {:?}",
            a.get("reasoning_content")
        );
    }

    #[test]
    fn build_request_preserves_reasoning_for_kimi_k2() {
        // kimi-k2 (a) returns 400 "reasoning_content is missing in assistant tool
        // call message" if the field is absent, AND (b) per kimi's docs preserves
        // historical reasoning for multi-step tool-use continuity. So kimi keeps the
        // REAL reasoning when present, and falls back to a "." stub only when absent.
        let p = OpenAIProvider::new("key", "moonshotai/kimi-k2").with_hints(ModelHints {
            fixed_temperature: true,
            ..Default::default()
        });
        // (a) real reasoning present -> preserved verbatim (tool-use continuity)
        let mut assistant = msg("the answer");
        assistant.role = MessageRole::Assistant;
        assistant.reasoning_content =
            Some("real prior reasoning kept for tool continuity".to_string());
        let msgs = [assistant];
        let v = serde_json::to_value(p.build_request(&msgs, &[], &ChatConfig::default(), false))
            .unwrap();
        let a = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message present");
        assert_eq!(
            a.get("reasoning_content").and_then(|r| r.as_str()),
            Some("real prior reasoning kept for tool continuity"),
            "kimi-k2 must preserve real prior reasoning_content for tool-use continuity"
        );
        // (b) no reasoning present -> "." stub to satisfy the 400 presence check
        let mut assistant2 = msg("the answer");
        assistant2.role = MessageRole::Assistant;
        assistant2.reasoning_content = None;
        let msgs2 = [assistant2];
        let v2 = serde_json::to_value(p.build_request(&msgs2, &[], &ChatConfig::default(), false))
            .unwrap();
        let a2 = v2["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message present");
        assert_eq!(
            a2.get("reasoning_content").and_then(|r| r.as_str()),
            Some("."),
            "kimi-k2 must get the \".\" stub when reasoning_content is absent"
        );
    }

    #[test]
    fn kimi_k3_hints_survive_official_endpoint_and_pin_temperature() {
        // K3 pins sampling params server-side -> never send temperature.
        assert!(ModelHints::detect("kimi-k3").fixed_temperature);
        // Unlike the DeepSeek-specific thinking toggle, K3's max-only
        // reasoning_effort is a plain top-level field, so with_base_url must
        // not downgrade it on the official moonshot endpoint.
        let p = OpenAIProvider::new("k", "kimi-k3")
            .with_provider_label("moonshot")
            .with_base_url("https://api.moonshot.ai/v1");
        assert_eq!(p.hints.reasoning_style, ReasoningStyle::EffortLowHighMax);
        // Explicit config override still wins (with_hints runs after
        // with_base_url), e.g. for a proxy that rejects reasoning_effort.
        let overridden = OpenAIProvider::new("k", "kimi-k3")
            .with_base_url("https://api.moonshot.ai/v1")
            .with_hints(ModelHints {
                reasoning_style: ReasoningStyle::None,
                fixed_temperature: true,
                ..Default::default()
            });
        assert_eq!(overridden.hints.reasoning_style, ReasoningStyle::None);
    }

    /// The Kimi coding plan (family `moonshot-coding`) exposes K3 under the bare
    /// ids `k3` / `k3-256k` / `kimi-for-coding*`, which don't contain `kimi-k3`.
    /// They MUST still pin temperature (else the endpoint 400s "only 1 is
    /// allowed") and get K3's max-only reasoning.
    #[test]
    fn coding_plan_k3_ids_pin_temperature_and_max_reasoning() {
        for id in [
            "k3",
            "k3-256k",
            "kimi-for-coding",
            "kimi-for-coding-highspeed",
        ] {
            let h = ModelHints::detect(id);
            assert!(
                h.fixed_temperature,
                "{id} must pin temperature (K3 rejects any temperature != 1)"
            );
            // These ids ARE the K3 model, so they must also get K3's graded
            // low|high|max reasoning — otherwise `/thinking` is silently a
            // no-op for the coding-plan aliases even though temperature is pinned.
            assert_eq!(
                h.reasoning_style,
                ReasoningStyle::EffortLowHighMax,
                "{id} is the K3 model and must get K3's graded low|high|max reasoning"
            );
        }
        // Guard: an unrelated model containing "k3" as a substring is NOT the
        // coding plan (exact match only), so it is unaffected.
        assert!(!ModelHints::detect("mock-k3000").fixed_temperature);
    }

    #[test]
    fn reasoning_emission_for_k3_is_graded_and_for_glm_is_a_toggle() {
        use crate::config::ReasoningEffort as RE;
        let build = |model: &str, effort: Option<RE>| {
            let p = OpenAIProvider::new("key", model);
            let cfg = ChatConfig {
                reasoning_effort: effort,
                ..ChatConfig::default()
            };
            serde_json::to_value(p.build_request(&[msg("hi")], &[], &cfg, false)).unwrap()
        };

        // Kimi K3: graded low|high|max (no medium tier → clamps up to high; Max
        // stays "max"). No `thinking` object (K3 rejects it).
        for (effort, want) in [
            (RE::Low, "low"),
            (RE::Medium, "high"),
            (RE::High, "high"),
            (RE::Max, "max"),
        ] {
            let v = build("k3", Some(effort));
            assert_eq!(
                v["reasoning_effort"].as_str(),
                Some(want),
                "k3 {effort:?} must map to {want}, not collapse to max"
            );
            assert!(
                v.get("thinking").is_none_or(|t| t.is_null()),
                "k3 must NOT send a thinking object"
            );
        }
        // No effort configured → nothing emitted (K3 thinks by its server default).
        let v = build("k3", None);
        assert!(v.get("reasoning_effort").is_none_or(|r| r.is_null()));

        // GLM-5.2: any effort level ENABLES thinking via the binary toggle; it
        // must NOT send reasoning_effort (previously it emitted nothing at all).
        for effort in [RE::Low, RE::Medium, RE::High, RE::Max] {
            let v = build("glm-5.2", Some(effort));
            assert_eq!(
                v["thinking"],
                serde_json::json!({ "type": "enabled" }),
                "glm {effort:?} must enable thinking"
            );
            assert!(
                v.get("reasoning_effort").is_none_or(|r| r.is_null()),
                "glm must NOT send reasoning_effort"
            );
        }
        // No effort → nothing (server default).
        let v = build("glm-5.2", None);
        assert!(v.get("thinking").is_none_or(|t| t.is_null()));
    }

    #[test]
    fn kimi_k3_pins_temperature_and_never_sends_the_thinking_object() {
        // K3 always thinks and rejects the K2.x `thinking` object; it also pins
        // temperature server-side. (Graded low|high|max emission is covered by
        // `reasoning_emission_for_k3_is_graded_and_for_glm_is_a_toggle`.)
        let p = OpenAIProvider::new("key", "kimi-k3");
        let msgs = [msg("hi")];
        use crate::config::ReasoningEffort as RE;
        for effort in [RE::Low, RE::Medium, RE::High, RE::Max] {
            let cfg = ChatConfig {
                reasoning_effort: Some(effort),
                ..Default::default()
            };
            let v = serde_json::to_value(p.build_request(&msgs, &[], &cfg, false)).unwrap();
            assert!(
                v.get("thinking").is_none(),
                "kimi-k3 must not emit the K2.x thinking object"
            );
            assert!(
                v.get("temperature").is_none(),
                "kimi-k3 pins temperature server-side"
            );
        }
    }

    #[test]
    fn build_request_preserves_reasoning_for_kimi_k3() {
        // K3's quickstart mandates the same round-trip contract as kimi-k2:
        // "add the complete assistant message returned by the API to the next
        // request. Do not keep only `content`". Auto-detected hints (no
        // with_hints) must be enough to get it.
        let p = OpenAIProvider::new("key", "kimi-k3");
        let mut assistant = msg("the answer");
        assistant.role = MessageRole::Assistant;
        assistant.reasoning_content = Some("prior k3 reasoning".to_string());
        let msgs = [assistant];
        let v = serde_json::to_value(p.build_request(&msgs, &[], &ChatConfig::default(), false))
            .unwrap();
        let a = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message present");
        assert_eq!(
            a.get("reasoning_content").and_then(|r| r.as_str()),
            Some("prior k3 reasoning"),
            "kimi-k3 must round-trip prior assistant reasoning_content"
        );
        // Absent reasoning -> "." stub (same presence contract as kimi-k2).
        let mut assistant2 = msg("the answer");
        assistant2.role = MessageRole::Assistant;
        assistant2.reasoning_content = None;
        let msgs2 = [assistant2];
        let v2 = serde_json::to_value(p.build_request(&msgs2, &[], &ChatConfig::default(), false))
            .unwrap();
        let a2 = v2["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message present");
        assert_eq!(
            a2.get("reasoning_content").and_then(|r| r.as_str()),
            Some("."),
            "kimi-k3 must get the \".\" stub when reasoning_content is absent"
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

#[cfg(test)]
mod cache_usage_tests {
    //! OpenAI chat-completions caches long prompts automatically and reports
    //! the cached portion in `usage.prompt_tokens_details.cached_tokens`.
    //! Both the non-streaming and SSE paths must surface it as
    //! `TokenUsage::cache_read_tokens` so cache hits flow into the
    //! usage/cost pipeline. Compat providers that omit the field parse as 0.

    use super::*;
    use crate::config::ChatConfig;
    use octos_core::{Message, MessageRole};

    fn reasoning_usage_cases() -> Vec<(serde_json::Value, u32)> {
        use serde_json::json;
        [
            (Some(json!({"reasoning_tokens": 6})), 6),
            (Some(json!({"reasoning_tokens": 0})), 0),
            (None, 0),
            (Some(serde_json::Value::Null), 0),
            (Some(json!({})), 0),
        ]
        .into_iter()
        .map(|(details, expected)| {
            let mut usage = json!({
                "prompt_tokens": 17,
                "completion_tokens": 8,
                "prompt_tokens_details": {"cached_tokens": 7}
            });
            if let Some(details) = details {
                usage["completion_tokens_details"] = details;
            }
            (usage, expected)
        })
        .collect()
    }

    fn assert_reasoning_usage(usage: &TokenUsage, expected: u32) {
        assert_eq!(usage.reasoning_tokens, expected);
        // Reasoning is a component of completion_tokens, not extra output.
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_read_tokens, 7);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    #[test]
    fn should_preserve_reasoning_usage_from_sse_without_adding_to_output() {
        for (usage, expected) in reasoning_usage_cases() {
            let event = SseEvent {
                event: None,
                data: serde_json::json!({"choices": [], "usage": usage}).to_string(),
            };
            let events = parse_openai_sse_events(&event);
            let usage = events
                .iter()
                .find_map(|event| match event {
                    StreamEvent::Usage(usage) => Some(usage),
                    _ => None,
                })
                .expect("usage event");
            assert_reasoning_usage(usage, expected);
        }
    }

    #[tokio::test]
    async fn should_preserve_reasoning_usage_from_chat_without_adding_to_output() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for (usage, expected) in reasoning_usage_cases() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": "ok"},
                                 "finish_reason": "stop"}],
                    "usage": usage
                })))
                .expect(1)
                .mount(&server)
                .await;
            let provider = OpenAIProvider::new("fixture-fake-only", "fixture-model")
                .with_base_url(server.uri());
            let response = provider
                .chat(&[], &[], &ChatConfig::default())
                .await
                .unwrap();
            assert_eq!(response.content.as_deref(), Some("ok"));
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            assert_reasoning_usage(&response.usage, expected);
        }
    }

    #[test]
    fn should_parse_cached_tokens_from_sse_usage() {
        let event = SseEvent {
            event: None,
            data: r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":75}}}"#.into(),
        };
        let events = parse_openai_sse_events(&event);
        let usage = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .expect("usage event");
        // Normalized to disjoint accounting: OpenAI's prompt_tokens INCLUDES
        // cached_tokens, TokenUsage does not — total = input + cache_read.
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.cache_read_tokens, 75);
    }

    #[test]
    fn should_default_cached_tokens_to_zero_when_details_missing() {
        let event = SseEvent {
            event: None,
            data: r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#.into(),
        };
        let events = parse_openai_sse_events(&event);
        let usage = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .expect("usage event");
        assert_eq!(usage.cache_read_tokens, 0);
    }

    #[test]
    fn should_parse_cache_write_tokens_from_sse_usage_disjointly() {
        let event = SseEvent {
            event: None,
            data: r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":20,"cache_write_tokens":30}}}"#.into(),
        };
        let events = parse_openai_sse_events(&event);
        let usage = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(usage),
                _ => None,
            })
            .expect("usage event");
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.cache_read_tokens, 20);
        assert_eq!(usage.cache_write_tokens, 30);
    }

    #[tokio::test]
    async fn should_parse_cached_tokens_from_chat_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(
                        r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":75}}}"#,
                    )
                    .append_header("Content-Type", "application/json"),
            )
            .mount(&server)
            .await;

        let provider = OpenAIProvider::new("test-key", "gpt-4o").with_base_url(server.uri());
        let messages = vec![Message {
            role: MessageRole::User,
            content: "hi".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }];
        let response = provider
            .chat(&messages, &[], &ChatConfig::default())
            .await
            .unwrap();
        // Normalized to disjoint accounting: OpenAI's prompt_tokens INCLUDES
        // cached_tokens, TokenUsage does not — total = input + cache_read.
        assert_eq!(response.usage.input_tokens, 25);
        assert_eq!(response.usage.cache_read_tokens, 75);
    }
}

#[cfg(test)]
mod prompt_cache_affinity_tests {
    use super::*;

    fn affinity_config() -> ChatConfig {
        ChatConfig {
            prompt_cache_context: Some(crate::PromptCacheContext {
                affinity_key: "octos-stable-affinity".to_owned(),
                epoch_id: "epoch".to_owned(),
                stable_prefix_hash: "sha256:stable".to_owned(),
                semantic_boundaries: Vec::new(),
            }),
            ..Default::default()
        }
    }

    fn body(provider: &OpenAIProvider, config: &ChatConfig) -> serde_json::Value {
        serde_json::to_value(provider.build_request(&[Message::user("hello")], &[], config, false))
            .unwrap()
    }

    #[test]
    fn should_keep_explicit_affinity_opt_in_when_base_url_is_set_in_either_order() {
        let config = affinity_config();
        let opt_in_then_custom = OpenAIProvider::new("key", "kimi-k3")
            .with_prompt_cache_affinity(true)
            .with_base_url("https://api.moonshot.ai/v1");
        let custom_then_opt_in = OpenAIProvider::new("key", "kimi-k3")
            .with_base_url("https://api.moonshot.ai/v1")
            .with_prompt_cache_affinity(true);
        for provider in [opt_in_then_custom, custom_then_opt_in] {
            let body = body(&provider, &config);
            assert_eq!(
                body["prompt_cache_key"], "octos-stable-affinity",
                "an explicit opt-in must survive builder call ordering: {body}"
            );
        }
    }

    #[test]
    fn should_keep_explicit_affinity_opt_out_when_official_base_url_is_set_afterwards() {
        let config = affinity_config();
        let provider = OpenAIProvider::new("key", "gpt-5")
            .with_prompt_cache_affinity(false)
            .with_base_url("https://api.openai.com/v1");
        let body = body(&provider, &config);
        assert!(body.get("prompt_cache_key").is_none(), "{body}");
    }

    #[test]
    fn should_honor_kill_switch_at_request_time_when_flipped_after_construction() {
        let config = affinity_config();
        // Same constructed provider, kill-switch flipped between requests:
        // the decision must be made per request (as the Responses provider
        // does), not baked in at construction.
        let provider = OpenAIProvider::new("key", "gpt-5");
        assert_eq!(
            provider.prompt_cache_key_for(&config, true),
            Some("octos-stable-affinity")
        );
        assert_eq!(
            provider.prompt_cache_key_for(&config, false),
            None,
            "the operator kill-switch must be honored on the next request"
        );
        // An explicit opt-in is still subject to the operator kill-switch.
        let opted_in = OpenAIProvider::new("key", "kimi-k3")
            .with_base_url("https://api.moonshot.ai/v1")
            .with_prompt_cache_affinity(true);
        assert_eq!(
            opted_in.prompt_cache_key_for(&config, true),
            Some("octos-stable-affinity")
        );
        assert_eq!(opted_in.prompt_cache_key_for(&config, false), None);
    }

    #[test]
    fn should_treat_trailing_slash_official_base_url_as_official() {
        let config = affinity_config();
        let provider =
            OpenAIProvider::new("key", "gpt-5").with_base_url("https://api.openai.com/v1/");
        let body = body(&provider, &config);
        assert_eq!(
            body["prompt_cache_key"], "octos-stable-affinity",
            "a trailing slash must not disable official affinity: {body}"
        );
        assert_eq!(
            provider.provider_name(),
            "openai",
            "the official endpoint must not be tagged as a custom host"
        );
    }
}

#[cfg(test)]
mod lane_attributed_operational_errors {
    use octos_core::Message;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::OpenAIProvider;
    use crate::config::ChatConfig;
    use crate::error::{LlmError, LlmErrorKind};
    use crate::provider::LlmProvider;
    use crate::provider::test_lanes::assert_error_names_lane;
    use crate::retry::RetryProvider;

    const LANE: &str = "moonshot-coding@api/k3";
    const STYLE: &str = "api_style=openai_chat_completions";
    const FORBIDDEN: &[&str] = &["OpenAI response", "OpenAI request"];

    async fn k3_lane_returning(status: u16, body: &str) -> (MockServer, OpenAIProvider) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body.to_owned()))
            .mount(&server)
            .await;
        let provider = OpenAIProvider::new("key", "k3")
            .with_base_url(server.uri())
            .with_provider_label("moonshot-coding@api");
        (server, provider)
    }

    #[tokio::test]
    async fn should_name_k3_lane_when_response_body_is_malformed() {
        let (_server, provider) = k3_lane_returning(200, "not json{").await;
        let err = provider
            .chat(&[Message::user("hi")], &[], &ChatConfig::default())
            .await
            .unwrap_err();
        assert_error_names_lane(&err, LANE, STYLE, FORBIDDEN);
    }

    #[tokio::test]
    async fn should_name_k3_lane_when_choices_are_empty() {
        let (_server, provider) = k3_lane_returning(
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

    #[tokio::test]
    async fn should_name_k3_lane_with_api_style_when_status_error_is_mapped() {
        let (_server, provider) = k3_lane_returning(503, "upstream exploded").await;
        let err = provider
            .chat(&[Message::user("hi")], &[], &ChatConfig::default())
            .await
            .unwrap_err();
        assert_error_names_lane(&err, LANE, STYLE, FORBIDDEN);
        let llm = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<LlmError>())
            .expect("status errors stay typed");
        assert_eq!(llm.kind, LlmErrorKind::ServerError { status: 503 });
        assert_eq!(
            llm.provider, LANE,
            "the HarnessError lane label is unchanged"
        );
        assert!(RetryProvider::should_failover(&err));
        assert!(RetryProvider::is_retryable_error(&err));
    }
}
