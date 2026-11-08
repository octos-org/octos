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

    /// Model takes images but not video (`video_url` parts stripped, the
    /// file named in a note instead). Not inferred from the model name, for
    /// the same reason as `lacks_vision`: the graceful fallback learns it
    /// from the endpoint's refusal. Config-overridable to skip that attempt.
    #[serde(default)]
    pub lacks_video: bool,

    /// Keep tool-returned images in subsequent requests until their transcript
    /// rows are compacted away. Opt-in: retains visual context and a stable
    /// request prefix, at the cost of provider image/context usage. Video keeps
    /// its current-batch-only behavior. Use immutable/versioned image paths.
    #[serde(default)]
    pub retain_tool_images: bool,

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
            lacks_video: false,
            retain_tool_images: false,
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
        let reasoning_style = if m.contains("deepseek-v4")
            || m.contains("deepseek-reasoner")
            || m == "deepseek-flash"
            || m.ends_with("/deepseek-flash")
        {
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
            lacks_video: false,
            retain_tool_images: false,
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
            .inspect_err(|error| crate::provider::log_android_transport_failure(error, false))
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
            .inspect_err(|error| crate::provider::log_android_transport_failure(error, true))
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
        self.build_request_stripping(messages, tools, config, force_text_only, false)
    }

    /// `build_request` with the retry legs spelled out: `force_text_only`
    /// strips images (and video), `force_no_video` strips only the video
    /// parts — the leg for an endpoint that takes images but answered a
    /// `video_url` part with a refusal.
    fn build_request_stripping<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
        config: &'a ChatConfig,
        force_text_only: bool,
        force_no_video: bool,
    ) -> OpenAIRequest<'a> {
        // Effective content hints: honour the configured `lacks_vision` /
        // `lacks_video`, and additionally strip on the retry legs.
        let mut content_hints = self.hints.clone();
        content_hints.lacks_vision = content_hints.lacks_vision || force_text_only;
        content_hints.lacks_video = content_hints.lacks_video || force_text_only || force_no_video;
        // #2480: re-run the tool's symlink-ancestor walk at request build for
        // media paths inside the validated workspace.
        let scope_root = config.media_scope_root.as_deref();
        let mut openai_messages: Vec<OpenAIMessage> = Vec::with_capacity(messages.len() + 1);
        // Media a tool in the current batch handed the model. The chat
        // completions protocol takes no media in a tool message, so it goes
        // out as ONE user turn after the batch's tool outputs — built here,
        // on the wire only, never persisted — naming the calls it answers.
        let mut pending_media: Vec<OpenAIContentPart> = Vec::new();
        let mut pending_notes: Vec<String> = Vec::new();
        for (index, m) in messages.iter().enumerate() {
            // Drop empty assistant messages (no content, no tool_calls) —
            // these can appear in session history and cause 400 errors.
            if m.role == MessageRole::Assistant
                && m.content.is_empty()
                && m.tool_calls.as_ref().is_none_or(|tc| tc.is_empty())
            {
                continue;
            }
            if m.role != MessageRole::Tool && !pending_media.is_empty() {
                openai_messages.push(media_turn(&mut pending_media, &mut pending_notes));
            }
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
            // Preserve provider continuation fields according to the selected
            // protocol, not a blanket assumption that prior reasoning is waste.
            // DeepSeek's tool-enabled API requires prior reasoning on ALL
            // assistant rows, including answers without tool calls:
            // https://api-docs.deepseek.com/guides/thinking_mode/
            // Without tools, DeepSeek ignores that field. The route's hints
            // remain authoritative for compatible/non-official endpoints.
            //
            // With thinking enabled kimi-k2 (a)
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
            // containing "kimi-k2"/"kimi-k3". Only Kimi receives a fabricated
            // stub; missing DeepSeek history stays missing rather than being
            // replaced with invented reasoning.
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
            let deepseek_tool_continuation = self.hints.reasoning_style
                == ReasoningStyle::EffortAndThinkingToggle
                && !tools.is_empty();
            let reasoning = if role == "assistant" && deepseek_tool_continuation {
                m.reasoning_content.as_deref()
            } else if role == "assistant" && needs_reasoning_stub {
                match m.reasoning_content.as_deref() {
                    Some(r) if !r.is_empty() => Some(r),
                    _ => Some("."),
                }
            } else {
                None
            };

            let mut content = build_openai_content(m, &content_hints, scope_root);
            if m.role == MessageRole::Tool {
                let shown = crate::tool_media::for_tool_row_with_image_history(
                    messages,
                    index,
                    content_hints.lacks_vision,
                    content_hints.lacks_video,
                    content_hints.retain_tool_images,
                );
                if let Some(note) = shown.note.as_deref() {
                    content = append_note(content, note);
                }
                let call = m.tool_call_id.as_deref().unwrap_or("unknown");
                let mut rendered = Vec::new();
                for path in &shown.images {
                    match vision::encode_image(path, scope_root) {
                        Ok((mime, data)) => {
                            pending_media.push(OpenAIContentPart::ImageUrl {
                                image_url: OpenAIImageUrl {
                                    url: format!("data:{mime};base64,{data}"),
                                },
                            });
                            rendered.push(path.clone());
                        }
                        Err(_) => {
                            content =
                                append_note(content, &crate::tool_media::unreadable_note(path))
                        }
                    }
                }
                for path in &shown.videos {
                    match vision::encode_video(path, scope_root) {
                        Ok((mime, data)) => {
                            pending_media.push(OpenAIContentPart::VideoUrl {
                                video_url: OpenAIVideoUrl {
                                    url: format!("data:{mime};base64,{data}"),
                                },
                            });
                            rendered.push(path.clone());
                        }
                        Err(_) => {
                            content =
                                append_note(content, &crate::tool_media::unreadable_note(path))
                        }
                    }
                }
                if !rendered.is_empty() {
                    pending_notes.push(crate::tool_media::shown_note(call, &rendered));
                }
            }
            openai_messages.push(OpenAIMessage {
                role,
                content,
                reasoning_content: reasoning,
                tool_call_id: m.tool_call_id.as_deref(),
                tool_calls,
            });
        }
        if !pending_media.is_empty() {
            openai_messages.push(media_turn(&mut pending_media, &mut pending_notes));
        }

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
        // The same for video: an image-capable endpoint that does not take
        // `video_url` (DeepSeek answers 422 naming the part) gets the request
        // again with the video parts replaced by a note, so the turn proceeds
        // and the model is told what it did not get to see.
        let status = response.status().as_u16();
        if (status == 400 || status == 422) && request_has_user_media(messages, &self.hints) {
            let body = response.text().await.unwrap_or_default();
            let fail_fast = crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast;
            let retry = if !fail_fast
                && request_has_user_videos(messages, &self.hints)
                && is_video_modality_error(&body)
            {
                tracing::warn!(
                    provider = %self.provider_label,
                    model = %self.model,
                    status,
                    "endpoint rejected video content; retrying without video"
                );
                Some(self.build_request_stripping(messages, tools, config, false, true))
            } else if !fail_fast
                && request_has_user_images(messages, &self.hints)
                && is_image_modality_error(&body)
            {
                tracing::warn!(
                    provider = %self.provider_label,
                    model = %self.model,
                    "endpoint rejected image content (400); retrying text-only"
                );
                Some(self.build_request(messages, tools, config, true))
            } else {
                None
            };
            match retry {
                Some(retry) => {
                    self.trace_prompt_cache_input(&retry, config);
                    response = self.post_chat(&retry).await?;
                }
                None => {
                    let body = crate::provider::truncate_error_body(&body);
                    return Err(crate::error::LlmError::from_status_with_label(
                        status,
                        &body,
                        format!("{}/{}", self.provider_label, self.model),
                    )
                    .with_api_style(crate::provider::ApiStyle::OpenAiChatCompletions)
                    .into());
                }
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

        let truncated = choice.finish_reason == "length";
        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| {
                let call_id = tc.id;
                let name = tc.function.name;
                let arguments = serde_json::from_str(&tc.function.arguments).map_err(|error| {
                    let detail = format!(
                        "{} malformed tool arguments: {error}",
                        self.operational_message(crate::provider::OperationalStage::ParseResponse)
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
        let status = response.status().as_u16();
        if (status == 400 || status == 422) && request_has_user_media(messages, &self.hints) {
            let text = response.text().await.unwrap_or_default();
            let fail_fast = crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast;
            let retry = if !fail_fast
                && request_has_user_videos(messages, &self.hints)
                && is_video_modality_error(&text)
            {
                tracing::warn!(
                    provider = %self.provider_label,
                    model = %self.model,
                    status,
                    "endpoint rejected video content; retrying without video (stream)"
                );
                Some(self.build_request_stripping(messages, tools, config, false, true))
            } else if !fail_fast
                && request_has_user_images(messages, &self.hints)
                && is_image_modality_error(&text)
            {
                tracing::warn!(
                    provider = %self.provider_label,
                    model = %self.model,
                    "endpoint rejected image content (400); retrying text-only (stream)"
                );
                Some(self.build_request(messages, tools, config, true))
            } else {
                None
            };
            if let Some(retry) = retry {
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
    /// A video as a data URL. The part the multimodal OpenAI-compatible
    /// endpoints take (GLM's coding endpoint, Kimi); a text-or-image-only
    /// endpoint rejects the request naming `video_url`, and the retry in
    /// `chat()` / `chat_stream()` then resends without it.
    #[serde(rename = "video_url")]
    VideoUrl { video_url: OpenAIVideoUrl },
}

#[derive(Serialize)]
struct OpenAIImageUrl {
    url: String,
}

#[derive(Serialize)]
struct OpenAIVideoUrl {
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
    !hints.lacks_vision && request_has_media(messages, vision::is_image, hints.retain_tool_images)
}

/// A user row's media, or a current-batch tool row's: both go out inline.
fn request_has_media(messages: &[Message], kind: fn(&str) -> bool, retain_history: bool) -> bool {
    messages.iter().enumerate().any(|(i, m)| {
        m.media.iter().any(|p| kind(p))
            && (m.role == MessageRole::User
                || (m.role == MessageRole::Tool
                    && (retain_history || crate::tool_media::is_current(messages, i))))
    })
}

/// Whether the request carries a user-row video we would have sent as a
/// `video_url` part. Decides whether a refusal is worth retrying without it.
fn request_has_user_videos(messages: &[Message], hints: &ModelHints) -> bool {
    !hints.lacks_video
        && !hints.lacks_vision
        && request_has_media(messages, vision::is_video, false)
}

fn request_has_user_media(messages: &[Message], hints: &ModelHints) -> bool {
    request_has_user_images(messages, hints) || request_has_user_videos(messages, hints)
}

/// A refusal of the `video_url` part specifically. DeepSeek: 422 `unknown
/// variant `video_url`, expected one of `text`, `image_url`, `file``; other
/// endpoints say the modality or the part is not supported.
fn is_video_modality_error(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("video_url")
        || (b.contains("video")
            && (b.contains("not support") || b.contains("modal") || b.contains("unsupported")))
}

fn build_openai_content(
    msg: &Message,
    hints: &ModelHints,
    scope_root: Option<&std::path::Path>,
) -> Option<OpenAIContent> {
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
    // not pushed unsolicited into vision content. A tool that wants the
    // model to look at something hands it over as `model_media`, which the
    // agent loop turns into a user row (see `execute_tools`).
    let images: Vec<_> = if hints.lacks_vision || msg.role != MessageRole::User {
        vec![]
    } else {
        msg.media.iter().filter(|p| vision::is_image(p)).collect()
    };
    // Video rides on the same user rows as a `video_url` part, for the
    // endpoints that take one (GLM's coding endpoint, Kimi). An endpoint
    // that does not answers with a refusal naming the part and the retry
    // leg rebuilds with `lacks_video`, which lands here as a note.
    let videos: Vec<_> = if hints.lacks_vision || hints.lacks_video || msg.role != MessageRole::User
    {
        vec![]
    } else {
        msg.media.iter().filter(|p| vision::is_video(p)).collect()
    };

    if images.is_empty() && videos.is_empty() {
        // Build a note for any media the LLM won't see inline:
        // - Non-image files (CSV, PDF, etc.) → include full path so agent can read_file
        // - Images stripped because model lacks vision → include filename
        // - Videos stripped because the model takes no video → include filename
        let non_image_files: Vec<_> = msg
            .media
            .iter()
            .filter(|p| !vision::is_image(p) && !vision::is_video(p))
            .collect();
        let stripped_images = hints.lacks_vision && msg.media.iter().any(|p| vision::is_image(p));
        let stripped_videos = msg.role == MessageRole::User
            && (hints.lacks_vision || hints.lacks_video)
            && msg.media.iter().any(|p| vision::is_video(p));

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
        // A video the model cannot view is named, not silently dropped: the
        // model should say it could not watch it rather than guess.
        let video_note = if stripped_videos {
            let names: Vec<String> = msg
                .media
                .iter()
                .filter(|p| vision::is_video(p))
                .map(|p| {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone())
                })
                .collect();
            Some(format!(
                "[video attachments this model cannot view: {}. Say so if asked about them; do not guess their contents.]",
                names.join(", ")
            ))
        } else {
            None
        };
        let media_note = match (media_note, video_note) {
            (Some(a), Some(b)) => Some(format!("{a}\n{b}")),
            (a, b) => a.or(b),
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
        if let Ok((mime, data)) = vision::encode_image(path, scope_root) {
            parts.push(OpenAIContentPart::ImageUrl {
                image_url: OpenAIImageUrl {
                    url: format!("data:{mime};base64,{data}"),
                },
            });
        }
    }
    for path in videos {
        if let Ok((mime, data)) = vision::encode_video(path, scope_root) {
            parts.push(OpenAIContentPart::VideoUrl {
                video_url: OpenAIVideoUrl {
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

/// Append a note line to a message's content, whatever shape it has.
fn append_note(content: Option<OpenAIContent>, note: &str) -> Option<OpenAIContent> {
    Some(match content {
        Some(OpenAIContent::Parts(mut parts)) => {
            parts.push(OpenAIContentPart::Text {
                text: note.to_string(),
            });
            OpenAIContent::Parts(parts)
        }
        Some(OpenAIContent::Text(text)) => {
            OpenAIContent::Text(crate::tool_media::with_note(&text, Some(note)))
        }
        None => OpenAIContent::Text(note.to_string()),
    })
}

/// The user turn that carries a tool batch's media (see `build_request`).
fn media_turn<'a>(
    media: &mut Vec<OpenAIContentPart>,
    notes: &mut Vec<String>,
) -> OpenAIMessage<'a> {
    let mut parts = std::mem::take(media);
    parts.push(OpenAIContentPart::Text {
        text: std::mem::take(notes).join("\n"),
    });
    OpenAIMessage {
        role: "user",
        content: Some(OpenAIContent::Parts(parts)),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    }
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
        Err(e) => {
            return vec![StreamEvent::Error(format!(
                "invalid OpenAI-compatible SSE JSON: {e}"
            ))];
        }
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
            source: None,
            participant: None,
        }
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

    #[test]
    fn should_preserve_deepseek_reasoning_when_tools_are_sent() {
        let tools = [ToolSpec {
            name: "read_file".into(),
            description: "Read a workspace file".into(),
            input_schema: serde_json::json!({"type":"object","properties":{}}),
        }];
        for model in ["deepseek-flash", "deepseek-v4-flash", "deepseek-v4-pro"] {
            let provider = OpenAIProvider::new("key", model);
            let mut assistant = msg("Prior answer without a tool call");
            assistant.role = MessageRole::Assistant;
            assistant.reasoning_content = Some("retained provider reasoning\n精确保留".into());
            // Exercise transcript serialization too: resumed history must keep
            // the actual provider field, including on a prior non-tool answer.
            let assistant: Message =
                serde_json::from_value(serde_json::to_value(assistant).unwrap()).unwrap();
            let messages = [assistant, msg("Continue with the available tools")];
            let value = serde_json::to_value(provider.build_request(
                &messages,
                &tools,
                &ChatConfig::default(),
                false,
            ))
            .unwrap();
            assert_eq!(
                value["messages"][0]["reasoning_content"], "retained provider reasoning\n精确保留",
                "{model}"
            );
        }
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
            source: None,
            participant: None,
        }
    }

    fn msg_with_user_video(dir: &std::path::Path) -> Message {
        let clip = dir.join("clip.mp4");
        std::fs::write(&clip, b"\x00\x00\x00\x18ftypisom").unwrap();
        Message {
            role: MessageRole::User,
            content: "what happens in this clip".to_string(),
            media: vec![clip.to_string_lossy().into_owned()],
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

    /// DeepSeek's actual refusal of a `video_url` part.
    const VIDEO_PART_422_BODY: &str = r#"{"error":{"message":"Failed to deserialize the JSON body into the target type: messages[0]: unknown variant `video_url`, expected one of `text`, `image_url`, `file` at line 1 column 6377","type":"invalid_request_error"}}"#;

    #[tokio::test]
    async fn should_retry_without_video_when_the_endpoint_refuses_the_video_part() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        // The first request carries the video part and is refused; the
        // retry, without it, is answered.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains("video_url"))
            .respond_with(
                ResponseTemplate::new(422)
                    .set_body_string(VIDEO_PART_422_BODY)
                    .append_header("Content-Type", "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "I could not watch it."}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider =
            OpenAIProvider::new("test-key", "deepseek-v4-flash").with_base_url(server.uri());
        let messages = vec![msg_with_user_video(dir.path())];
        let response = provider
            .chat(&messages, &[], &ChatConfig::default())
            .await
            .expect("the retry without video succeeds");
        assert_eq!(response.content.as_deref(), Some("I could not watch it."));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let second = String::from_utf8_lossy(&requests[1].body);
        assert!(
            !second.contains("video_url"),
            "retry must carry no video part"
        );
        assert!(
            second.contains("video attachments this model cannot view"),
            "retry names the video it dropped: {second}"
        );
    }

    /// #2480 end to end: a tool validated `project/img.png` inside the
    /// workspace, then a background writer swapped `project` for a symlink
    /// to elsewhere before the request was built. The request body is the
    /// last stop before the bytes leave the machine — it must carry the
    /// validated image on the untouched layout and never the swapped one.
    #[cfg(unix)]
    #[test]
    fn build_request_never_ships_bytes_behind_a_swapped_symlink_ancestor() {
        let ws = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        let project = ws.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let legit_path = project.join("img.png");
        let legit_bytes: Vec<u8> =
            [b"\x89PNG\r\n\x1a\n".as_slice(), b"the-real-screenshot"].concat();
        std::fs::write(&legit_path, &legit_bytes).unwrap();
        let payload_path = secret.path().join("img.png");
        let payload_bytes: Vec<u8> =
            [b"\x89PNG\r\n\x1a\n".as_slice(), b"swapped-private-bytes"].concat();
        std::fs::write(&payload_path, &payload_bytes).unwrap();

        let b64 = |bytes: &[u8]| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };

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
            source: None,
            participant: None,
        };
        let mut assistant = mk(MessageRole::Assistant, "");
        assistant.tool_calls = Some(vec![octos_core::ToolCall {
            id: "call_1".into(),
            name: "view_image".into(),
            arguments: serde_json::json!({"path": "img.png"}),
            metadata: None,
        }]);
        let mut tool = mk(MessageRole::Tool, "{\"format\":\"png\"}");
        tool.tool_call_id = Some("call_1".into());
        tool.media = vec![legit_path.to_string_lossy().into_owned()];
        let msgs = vec![mk(MessageRole::User, "look"), assistant, tool];

        let p = OpenAIProvider::new("key", "gpt-4o");
        let cfg = ChatConfig {
            media_scope_root: Some(ws.path().to_path_buf()),
            ..Default::default()
        };

        // Untouched layout: the validated image ships.
        let body = serde_json::to_string(&p.build_request(&msgs, &[], &cfg, false)).unwrap();
        assert!(
            body.contains(&b64(&legit_bytes)),
            "validated image ships: {body}"
        );

        // The swap: `project` now points at the secret directory.
        std::fs::remove_dir_all(&project).unwrap();
        std::os::unix::fs::symlink(secret.path(), &project).unwrap();
        let body = serde_json::to_string(&p.build_request(&msgs, &[], &cfg, false)).unwrap();
        assert!(
            !body.contains(&b64(&payload_bytes)),
            "swapped bytes must never leave the machine: {body}"
        );
        assert!(
            body.contains("could not be read"),
            "the model is told the media went away: {body}"
        );
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

    #[test]
    fn should_surface_invalid_openai_sse_json() {
        let event = SseEvent {
            event: None,
            data: "{not-json".into(),
        };
        let events = parse_openai_sse_events(&event);
        assert!(
            matches!(events.as_slice(), [StreamEvent::Error(msg)]
                if msg.contains("OpenAI-compatible SSE JSON") && msg.contains("key must be a string")),
            "{events:?}"
        );
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
}

#[cfg(test)]
mod lane_attributed_operational_errors {
    use octos_core::Message;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::OpenAIProvider;
    use crate::config::ChatConfig;

    use crate::provider::LlmProvider;

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
    async fn should_reject_malformed_tool_arguments_with_call_context() {
        let (_server, provider) = k3_lane_returning(
            200,
            r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_good","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}},{"id":"call_bad","type":"function","function":{"name":"shell","arguments":"{bad"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        )
        .await;
        let error = match provider
            .chat(&[Message::user("hi")], &[], &ChatConfig::default())
            .await
        {
            Err(error) => error,
            Ok(response) => panic!(
                "mixed malformed tool call must not return ToolUse: {:?}",
                response.stop_reason
            ),
        };
        let detail = format!("{error:#}");
        assert!(detail.contains("moonshot-coding@api/k3"), "{detail}");
        assert!(detail.contains("call_bad"), "{detail}");
        let typed = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<crate::error::StreamError>())
            .expect("agent recovery requires typed malformed arguments");
        match typed {
            crate::error::StreamError::MalformedArgs {
                tool_id, tool_name, ..
            } => {
                assert_eq!(tool_id, "call_bad");
                assert_eq!(tool_name, "shell");
            }
            other => panic!("expected MalformedArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_classify_length_truncated_tool_arguments_as_retryable_truncation() {
        let (_server, provider) = k3_lane_returning(
            200,
            r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_bad","type":"function","function":{"name":"shell","arguments":"{bad"}}]},"finish_reason":"length"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        )
        .await;
        let error = provider
            .chat(&[Message::user("hi")], &[], &ChatConfig::default())
            .await
            .unwrap_err();
        let typed = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<crate::error::StreamError>())
            .expect("length-truncated JSON must stay typed");
        match typed {
            crate::error::StreamError::TruncatedToolCall { tool_id, .. } => {
                assert_eq!(tool_id, "call_bad")
            }
            other => panic!("expected TruncatedToolCall, got {other:?}"),
        }
        assert!(format!("{error:#}").contains("moonshot-coding@api/k3"));
    }
}
