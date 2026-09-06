//! Anthropic (Claude) provider implementation.

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
use crate::config::ReasoningEffort;
use crate::provider::{LlmProvider, endpoint_label_from_base_url};
use crate::types::{
    ChatResponse, ChatStream, ProviderMetadata, StopReason, StreamEvent, TokenUsage, ToolSpec,
};

/// Anthropic Claude provider.
pub struct AnthropicProvider {
    client: Client,
    /// Separate client for streaming requests, built without a total request
    /// timeout so a healthy long generation is never cut off mid-stream. See
    /// [`crate::provider::build_streaming_http_client`].
    stream_client: Client,
    api_key: SecretString,
    model: String,
    base_url: String,
    /// Label for logs/failover. Defaults to `"anthropic"` but overridden by
    /// registry entries (e.g. `"zai"`, `"r9s"`) so providers are
    /// distinguishable in failover chains.
    provider_label: String,
    /// Emit `cache_control: {"type": "ephemeral"}` breakpoints so Anthropic
    /// serves the replayed prefix from its prompt cache (~0.1x input rate on
    /// reads) instead of billing the whole conversation at full rate every
    /// round. The official endpoint defaults ON, while custom compatible
    /// endpoints require an explicit opt-in. The `OCTOS_PROMPT_CACHING` env
    /// kill-switch can force the official default off at startup — see
    /// [`Self::with_prompt_caching`] and [`prompt_caching_default`].
    prompt_caching: bool,
    /// Whether a builder call explicitly selected the prompt-caching mode.
    /// Custom Anthropic-compatible endpoints default to off, but an explicit
    /// override must survive either builder-call order.
    prompt_caching_override: Option<bool>,
}

const OFFICIAL_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

fn is_official_anthropic_base_url(base_url: &str) -> bool {
    base_url
        .trim()
        .trim_end_matches('/')
        .eq_ignore_ascii_case(OFFICIAL_ANTHROPIC_BASE_URL)
}

#[cfg(test)]
fn prompt_caching_default_for_base_url_from(base_url: &str, env_value: Option<&str>) -> bool {
    is_official_anthropic_base_url(base_url)
        && crate::cache_manifest::prompt_cache_features_enabled_from(env_value)
}

fn prompt_caching_default_for_base_url(base_url: &str) -> bool {
    is_official_anthropic_base_url(base_url) && prompt_cache_features_enabled()
}

/// Resolve the default prompt-caching state from a raw env value.
///
/// Caching stays ON unless the value is explicitly falsy (`0`, `false`,
/// `off`, `no`, case- and whitespace-insensitive). Unset, empty, or any
/// other value keeps the default ON. Pure over its input so the kill-switch
/// is unit-testable without mutating process env (the workspace is
/// `deny(unsafe_code)`, and `std::env::set_var` is `unsafe` on edition 2024).
#[cfg(test)]
fn prompt_caching_default_from(env_value: Option<&str>) -> bool {
    crate::cache_manifest::prompt_cache_features_enabled_from(env_value)
}

/// Default prompt-caching state, honoring the `OCTOS_PROMPT_CACHING`
/// kill-switch. See [`prompt_caching_default_from`].
fn prompt_caching_default() -> bool {
    prompt_caching_default_for_base_url(OFFICIAL_ANTHROPIC_BASE_URL)
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
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
            base_url: OFFICIAL_ANTHROPIC_BASE_URL.to_string(),
            provider_label: "anthropic".to_string(),
            prompt_caching: prompt_caching_default(),
            prompt_caching_override: None,
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
        if self.prompt_caching_override.is_none() {
            self.prompt_caching = prompt_caching_default_for_base_url(&self.base_url);
        }
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

    /// Override the provider label shown in logs and status display.
    pub fn with_provider_label(mut self, label: impl Into<String>) -> Self {
        self.provider_label = label.into();
        self
    }

    /// Toggle Anthropic prompt-cache breakpoints explicitly.
    ///
    /// When enabled the request carries three ephemeral `cache_control`
    /// breakpoints (Anthropic allows up to 4): the system-prompt block, the
    /// LAST tool definition, and the last content block of the LAST
    /// user-role message — caching the stable prefix (tools + system) plus
    /// the rolling conversation history across loop iterations.
    ///
    /// The official Anthropic endpoint defaults ON. Custom compatible
    /// endpoints default OFF because some reject `cache_control` or the
    /// block-array `system` form. Disabling restores the exact pre-caching
    /// wire shape (plain-string `system`, verbatim tools).
    ///
    /// Operators can flip the default OFF at startup without a rebuild via
    /// `OCTOS_PROMPT_CACHING=0` (see [`prompt_caching_default`]); this
    /// explicit builder still wins over the env default when called.
    pub fn with_prompt_caching(mut self, enabled: bool) -> Self {
        self.prompt_caching = enabled;
        self.prompt_caching_override = Some(enabled);
        self
    }

    fn operational_message(&self, stage: crate::provider::OperationalStage) -> String {
        crate::provider::operational_error_message(
            stage,
            &self.provider_label,
            &self.model,
            crate::provider::ApiStyle::AnthropicMessages,
        )
    }

    /// Build the shared request struct used by both chat() and chat_stream().
    fn build_request<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
        config: &'a ChatConfig,
    ) -> AnthropicRequest<'a> {
        let max_tokens = config.max_tokens.unwrap_or(4096);
        // Provider default AND the request did not opt out: a one-shot call
        // (`ChatConfig.cache_retention: None`) pays the 1.25x cache-write
        // premium on a prefix it never sends again, so it gets the exact
        // pre-caching wire shape instead (mirrors pi's `cacheRetention:
        // "none"` on summarization requests).
        let cache = (self.prompt_caching && config.cache_retention != crate::CacheRetention::None)
            .then_some(EPHEMERAL_CACHE_CONTROL);
        let mut api_messages = build_anthropic_messages(messages);
        if cache.is_some() {
            apply_message_cache_breakpoint(&mut api_messages);
        }
        let thinking = config
            .reasoning_effort
            .and_then(|effort| build_anthropic_thinking(effort, max_tokens));
        let (temperature, top_p, top_k) = self.sampling_fields(config);
        AnthropicRequest {
            model: &self.model,
            max_tokens,
            messages: api_messages,
            system: {
                let system_parts: Vec<&str> = messages
                    .iter()
                    .filter(|m| m.role == octos_core::MessageRole::System)
                    .map(|m| m.content.as_str())
                    .collect();
                if system_parts.is_empty() {
                    None
                } else {
                    let text = system_parts.join("\n\n");
                    Some(match cache {
                        // Block-array form: the only shape that can carry
                        // cache_control (Anthropic accepts both forms). An
                        // all-blank system stays in string form — an empty
                        // text BLOCK is rejected while `"system": ""` is not.
                        Some(cc) if !text.is_empty() => {
                            AnthropicSystem::Blocks(vec![AnthropicSystemBlock {
                                r#type: "text",
                                text,
                                cache_control: Some(cc),
                            }])
                        }
                        _ => AnthropicSystem::Text(text),
                    })
                }
            },
            tools: if tools.is_empty() {
                None
            } else {
                let last = tools.len() - 1;
                Some(
                    tools
                        .iter()
                        .enumerate()
                        .map(|(i, t)| AnthropicTool {
                            name: &t.name,
                            description: &t.description,
                            input_schema: &t.input_schema,
                            // One breakpoint on the LAST tool caches the
                            // whole (deterministically ordered) tool array.
                            cache_control: if i == last { cache } else { None },
                        })
                        .collect(),
                )
            },
            thinking,
            context_management: config.context_management.as_ref(),
            temperature,
            top_p,
            top_k,
            tool_choice: config.tool_choice.anthropic_wire(!tools.is_empty()),
        }
    }

    fn prompt_cache_input_manifest(
        &self,
        request: &AnthropicRequest<'_>,
        config: &ChatConfig,
    ) -> PromptCacheInputManifest {
        let normalized = without_cache_markers(
            serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
        );
        let mut stable = Vec::new();
        if let Some(system) = normalized.get("system") {
            stable.push(("system".to_owned(), system.clone()));
        }
        if let Some(tools) = normalized.get("tools").and_then(|value| value.as_array()) {
            stable.extend(
                tools
                    .iter()
                    .enumerate()
                    .map(|(index, tool)| (format!("tool:{index}"), tool.clone())),
            );
        }
        for key in ["thinking", "context_management", "tool_choice"] {
            if let Some(value) = normalized.get(key) {
                stable.push((format!("config:{key}"), value.clone()));
            }
        }
        let conversation = normalized
            .get("messages")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, message)| {
                let role = message
                    .get("role")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown");
                (format!("message:{index}:{role}"), message.clone())
            })
            .collect();
        PromptCacheInputManifest::from_normalized_segments(
            self.provider_label.clone(),
            self.model.clone(),
            config
                .prompt_cache_context
                .as_ref()
                .map(|context| context.epoch_id.as_str()),
            stable,
            conversation,
        )
    }

    fn trace_prompt_cache_input(&self, request: &AnthropicRequest<'_>, config: &ChatConfig) {
        if tracing::enabled!(target: "octos.prompt_cache", tracing::Level::TRACE) {
            self.prompt_cache_input_manifest(request, config).trace();
        }
    }

    /// Resolve the request's sampling fields (`temperature`, `top_p`,
    /// `top_k`) from the config, **model-capability-aware** (#2172 — before
    /// this, the whole Anthropic protocol path silently ignored both knobs;
    /// a first cut then over-corrected and forwarded them to every model,
    /// which 400s modern first-party Claude).
    ///
    /// Which knobs a model accepts is decided by [`model_accepts_sampling`],
    /// a default-DENY GLM allowlist: GLM via z.ai (`zai` / `zai-coding`)
    /// accepts the standard sampler set — the repetition-collapse knobs this
    /// change exists to deliver — while every other model accepts nothing on
    /// this path, including first-party Claude (Opus 4.7+/Sonnet 5 REMOVED
    /// `temperature`/`top_p`/`top_k` and 400 on them; we ship
    /// `claude-opus-4-7`) and any custom endpoint pointed here. A model that
    /// rejects a sampler never receives it.
    ///
    /// Within an accepting model:
    /// - `temperature` rides through only when non-zero. `Some(0.0)` is the
    ///   plumbing's built-in default (see the `build_chat_config` invariant
    ///   in octos-agent), indistinguishable from "unset" here, so it stays
    ///   off the wire and no-override requests remain byte-identical to the
    ///   pre-#2172 shape (prompt-cache prefixes depend on it). Near-greedy
    ///   decoding is available via e.g. `0.01`.
    /// - From `sampling_params`, only `top_p` / `top_k` exist on the
    ///   Anthropic Messages API; every other key (`repeat_penalty`,
    ///   `frequency_penalty`, `min_p`, …) is dropped with a `warn` naming it,
    ///   so operators learn why the knob did nothing. `sampling_params` is
    ///   operator-config-only (stock/internal flows leave it `None`), so the
    ///   warn never cries wolf.
    ///
    /// Rejecting-model suppression is logged at `debug`, not `warn`: stock
    /// octos reaches it without operator input (compaction and rich_output
    /// both set `temperature: 0.2` and can target a Claude model), so a warn
    /// would fire on ordinary turns.
    fn sampling_fields<'a>(
        &self,
        config: &'a ChatConfig,
    ) -> (
        Option<f32>,
        Option<&'a serde_json::Value>,
        Option<&'a serde_json::Value>,
    ) {
        let accepts = model_accepts_sampling(&self.model);

        // 0.0 is the built-in default sentinel; never emit it (see doc).
        let temperature = config.temperature.filter(|t| *t != 0.0);
        let mut top_p = None;
        let mut top_k = None;
        let mut dropped: Vec<&str> = Vec::new();
        if let Some(params) = &config.sampling_params {
            for (key, value) in params {
                // The guard folds capability into the match: on a rejecting
                // model `accepts` is false, so `top_p`/`top_k` fall through to
                // the drop arm exactly like an unknown key — a rejected
                // sampler can never reach the wire.
                match key.as_str() {
                    "top_p" if accepts => top_p = Some(value),
                    "top_k" if accepts => top_k = Some(value),
                    other => dropped.push(other),
                }
            }
        }
        if !dropped.is_empty() {
            tracing::warn!(
                provider = %self.provider_label,
                model = %self.model,
                dropped_keys = ?dropped,
                accepts_sampling = accepts,
                "sampling_params keys not accepted by this model on the \
                 Anthropic Messages API were dropped (first-party Claude \
                 accepts none; GLM via z.ai accepts only top_p / top_k)"
            );
        }

        if accepts {
            (temperature, top_p, top_k)
        } else {
            if temperature.is_some() {
                tracing::debug!(
                    provider = %self.provider_label,
                    model = %self.model,
                    temperature = ?temperature,
                    "model does not accept sampling params on the Anthropic \
                     Messages API: suppressing temperature (reverts to the \
                     pre-#2172 no-sampling wire)"
                );
            }
            (None, None, None)
        }
    }
}

/// Whether `model` accepts the standard sampler set (`temperature`, `top_p`,
/// `top_k`) on the Anthropic Messages API protocol (#2172).
///
/// **Default-DENY with a GLM allowlist.** The only class reached by this
/// provider that is known to accept sampling is GLM via z.ai (the `zai` /
/// `zai-coding` families, bare model `glm-*` — the repetition-collapse knobs
/// this change exists to deliver). Everything else returns `false`:
/// - **first-party Claude** (`anthropic`, and `r9s` claude-* proxies).
///   Modern Claude (Opus 4.7+, Sonnet 5, …) REMOVED `temperature`/`top_p`/
///   `top_k` from the Messages API and returns a hard 400 on them; we ship
///   `claude-opus-4-7` in `model_catalog.json`. Reverting Claude to the
///   pre-#2172 no-sampling wire is the safe direction — this PR was never
///   about tuning first-party sampling.
/// - **any other model an operator might point at this provider** — a
///   non-GLM OpenAI-path model, a self-hosted endpoint, the empty string.
///   origin/main forwarded nothing to any of them, so default-deny keeps
///   them no-worse-than-today (a default-ACCEPT gate would newly 400 a
///   custom endpoint that rejects sampling).
///
/// Matched on the normalized last path segment (lowercased; the segment after
/// the final `/`, since a custom base_url can pass a family-qualified
/// `anthropic/claude-*` or `vendor/glm-*`). Default-deny closes the qualified
/// bypass for free: only an affirmative `glm-` prefix accepts, so a
/// `claude-*` anywhere in the string — prefix or after a `/` — gets nothing.
/// The registry strips the catalog's `<family>/` prefix before construction
/// (`registry::mod` `split_once('/')`), so in practice the model arrives bare;
/// the segment split only guards the custom-base_url case.
///
/// Follow-ups deliberately out of scope (kept minimal and safe): a per-model
/// Claude carve-out to re-enable sampling on ≤4.6, a broader allowlist for
/// other Anthropic-compatible endpoints that accept sampling, and any
/// narrowing of the GLM set under extended thinking (GLM is treated as
/// accepting the full set; see the PR's stated assumption).
fn model_accepts_sampling(model: &str) -> bool {
    let leaf = model.rsplit('/').next().unwrap_or(model);
    leaf.trim().to_ascii_lowercase().starts_with("glm-")
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
        self.trace_prompt_cache_input(&request, config);

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
            .wrap_err_with(|| {
                crate::provider::transport_error_message(
                    false,
                    &self.provider_label,
                    &self.model,
                    crate::provider::ApiStyle::AnthropicMessages,
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let body = crate::provider::truncate_error_body(&body);
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &body,
                format!("{}/{}", self.provider_label, self.model),
            )
            .with_api_style(crate::provider::ApiStyle::AnthropicMessages)
            .into());
        }

        let api_response: AnthropicResponse = response.json().await.wrap_err_with(|| {
            self.operational_message(crate::provider::OperationalStage::ParseResponse)
        })?;

        Ok(anthropic_response_to_chat_response(api_response))
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let request = self.build_request(messages, tools, config);
        self.trace_prompt_cache_input(&request, config);

        let mut body = serde_json::to_value(&request).wrap_err_with(|| {
            self.operational_message(crate::provider::OperationalStage::SerializeRequest)
        })?;
        body.as_object_mut()
            .ok_or_else(|| {
                eyre::Report::msg(
                    self.operational_message(crate::provider::OperationalStage::BuildRequestBody),
                )
            })?
            .insert("stream".into(), true.into());

        // Stream client: no total timeout, so a long healthy generation is not
        // cut off. Stalls are bounded by the client's per-read timeout and the
        // agent's stream-timeout guards (see build_streaming_http_client).
        let response = self
            .stream_client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err_with(|| {
                crate::provider::transport_error_message(
                    true,
                    &self.provider_label,
                    &self.model,
                    crate::provider::ApiStyle::AnthropicMessages,
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let body = crate::provider::truncate_error_body(&text);
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &body,
                format!("{}/{}", self.provider_label, self.model),
            )
            .with_api_style(crate::provider::ApiStyle::AnthropicMessages)
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

    fn estimate_request_tokens(
        &self,
        messages: &[Message],
        tools: &[crate::types::ToolSpec],
    ) -> u32 {
        // #2143 part 3: the base estimate (messages + tool schemas) plus the
        // Anthropic request-envelope overhead the flat estimator omits — each
        // message is wrapped in a content-block array, system parts are lifted
        // into a separate top-level `system` array, and cache_control
        // breakpoints / tool_choice / metadata ride along. Conservative fixed
        // additions (they only ever OVER-count, so the route-fit guard never
        // under-estimates and lets an oversized request through).
        let base = crate::context::estimate_request_tokens_base(messages, tools);
        let per_message_framing = messages.len() as u32 * 4;
        const REQUEST_ENVELOPE_OVERHEAD: u32 = 24;
        base.saturating_add(per_message_framing)
            .saturating_add(REQUEST_ENVELOPE_OVERHEAD)
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        &self.provider_label
    }

    fn api_style(&self) -> Option<crate::provider::ApiStyle> {
        Some(crate::provider::ApiStyle::AnthropicMessages)
    }

    fn provider_metadata(&self) -> ProviderMetadata {
        let endpoint = if self.base_url != "https://api.anthropic.com" {
            endpoint_label_from_base_url(&self.base_url)
        } else {
            None
        };
        ProviderMetadata::new(self.provider_label.clone(), self.model.clone(), endpoint)
            .with_cache_lane(crate::types::CacheLane::Anthropic)
    }
}

/// The `cache_control: {"type": "ephemeral"}` marker Anthropic uses to place
/// prompt-cache breakpoints (max 4 per request; this provider emits 3).
#[derive(Serialize, Clone, Copy)]
struct AnthropicCacheControl {
    r#type: &'static str,
}

const EPHEMERAL_CACHE_CONTROL: AnthropicCacheControl = AnthropicCacheControl {
    r#type: "ephemeral",
};

/// Top-level `system` field. Anthropic accepts both a plain string and an
/// array of text blocks; only the block form can carry `cache_control`.
#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicSystem {
    /// Legacy plain-string form — emitted when prompt caching is disabled so
    /// odd Anthropic-compatible proxies see the exact pre-caching shape.
    Text(String),
    /// Block-array form carrying the cache breakpoint.
    Blocks(Vec<AnthropicSystemBlock>),
}

#[derive(Serialize)]
struct AnthropicSystemBlock {
    r#type: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<AnthropicCacheControl>,
}

/// [`ToolSpec`] plus the optional cache breakpoint. Field-for-field the same
/// wire shape as serializing `ToolSpec` verbatim, with `cache_control`
/// appended on the LAST tool only.
#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<AnthropicCacheControl>,
}

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<AnthropicSystem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    /// M8.5 tier 2: forwarded from `ChatConfig.context_management`. Opaque
    /// payload (typically `{ "edits": [ { "type":
    /// "clear_tool_uses_20250919", ... } ] }`) that tells Anthropic's server
    /// to clear old tool uses on its side. Only emitted when the field is
    /// non-null and the caller opted in via the builder.
    #[serde(skip_serializing_if = "Option::is_none")]
    context_management: Option<&'a serde_json::Value>,
    /// Operator temperature override (#2172). `None` both when unset and
    /// when the config carries the built-in `0.0` default sentinel — absent
    /// keeps the no-override wire byte-identical to the pre-#2172 shape.
    /// See [`AnthropicProvider::sampling_fields`].
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// `top_p` from `ChatConfig::sampling_params`, forwarded verbatim (#2172).
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<&'a serde_json::Value>,
    /// `top_k` from `ChatConfig::sampling_params`, forwarded verbatim (#2172).
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<&'a serde_json::Value>,
    /// `ChatConfig.tool_choice` on the wire (`{"type": "none"|"any"|"tool"}`);
    /// absent for the default `auto`. Anthropic invalidates message-level
    /// cache entries when this changes, so it is also a manifest segment.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
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
        ReasoningEffort::Disabled => 0,
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
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    #[serde(rename = "image")]
    Image {
        source: AnthropicImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    /// Prior assistant tool invocation, round-tripped from
    /// [`octos_core::Message::tool_calls`]. Anthropic requires the original
    /// `tool_use` block in the assistant turn for the following
    /// `tool_result` to pair with — without it the request 400s.
    /// (No `cache_control` field: message breakpoints only ever land on
    /// USER-role messages, and `tool_use` blocks are assistant-only.)
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
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
}

/// Place the rolling-history breakpoint: `cache_control` on the last content
/// block of the LAST user-role message (a plain user turn or the merged
/// tool_result batch — both serialize as role "user"). Combined with the
/// system/tools breakpoints this caches the whole replayed prefix; the next
/// round only pays full input rate for what was appended since.
///
/// The marker intentionally moves forward each round — Anthropic reuses the
/// longest previously-cached prefix (earlier breakpoints stay valid read
/// points within the TTL), so advancing the marker EXTENDS the cache rather
/// than invalidating it.
fn apply_message_cache_breakpoint(messages: &mut [AnthropicMessage<'_>]) {
    let Some(last_user_index) = last_complete_user_boundary(messages) else {
        return;
    };
    let last_user = &mut messages[last_user_index];
    match &mut last_user.content {
        AnthropicContent::Parts(parts) => match parts.last_mut() {
            Some(
                AnthropicContentBlock::Text { cache_control, .. }
                | AnthropicContentBlock::Image { cache_control, .. }
                | AnthropicContentBlock::ToolResult { cache_control, .. },
            ) => *cache_control = Some(EPHEMERAL_CACHE_CONTROL),
            // `tool_use` never appears in user messages; nothing to mark.
            Some(AnthropicContentBlock::ToolUse { .. }) | None => {}
        },
        AnthropicContent::Text(text) => {
            // An empty text BLOCK is rejected by Anthropic, so leave a
            // (degenerate) empty string message untouched.
            if text.is_empty() {
                return;
            }
            let text = std::mem::take(text);
            last_user.content = AnthropicContent::Parts(vec![AnthropicContentBlock::Text {
                text,
                cache_control: Some(EPHEMERAL_CACHE_CONTROL),
            }]);
        }
    }
}

/// Last user-role boundary at which every preceding tool-use has a result.
/// Tracking the full outstanding set prevents a plain user row after a
/// partially answered parallel batch from receiving a cache marker.
fn last_complete_user_boundary(messages: &[AnthropicMessage<'_>]) -> Option<usize> {
    let mut outstanding: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut boundary = None;
    for (index, message) in messages.iter().enumerate() {
        if let AnthropicContent::Parts(parts) = &message.content {
            for part in parts {
                match part {
                    AnthropicContentBlock::ToolUse { id, .. } => {
                        outstanding.insert(id.as_str());
                    }
                    AnthropicContentBlock::ToolResult { tool_use_id, .. } => {
                        outstanding.remove(tool_use_id.as_str());
                    }
                    AnthropicContentBlock::Text { .. } | AnthropicContentBlock::Image { .. } => {}
                }
            }
        }
        if message.role == "user" && outstanding.is_empty() {
            boundary = Some(index);
        }
    }
    boundary
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
            cache_control: None,
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
        cache_control: None,
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
                cache_control: None,
            });
        }
    }
    if !msg.content.is_empty() {
        parts.push(AnthropicContentBlock::Text {
            text: msg.content.clone(),
            cache_control: None,
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
    /// Tokens written to the prompt cache this request (billed ~1.25x the
    /// input rate). Absent from providers without caching — defaults to 0.
    #[serde(default)]
    cache_creation_input_tokens: u32,
    /// Tokens served from the prompt cache (billed ~0.1x the input rate).
    /// NOTE: Anthropic's `input_tokens` EXCLUDES cached tokens — the total
    /// prompt is `input + cache_read + cache_creation`.
    #[serde(default)]
    cache_read_input_tokens: u32,
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
            cache_read_tokens: api_response.usage.cache_read_input_tokens,
            cache_write_tokens: api_response.usage.cache_creation_input_tokens,
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
    cache_read_tokens: u32,
    cache_write_tokens: u32,
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
            let usage = &data["message"]["usage"];
            if let Some(t) = usage["input_tokens"].as_u64() {
                state.input_tokens = t as u32;
            }
            // Cache usage arrives on message_start (Anthropic reports it once
            // the prompt is processed, before any output tokens stream).
            if let Some(t) = usage["cache_read_input_tokens"].as_u64() {
                state.cache_read_tokens = t as u32;
            }
            if let Some(t) = usage["cache_creation_input_tokens"].as_u64() {
                state.cache_write_tokens = t as u32;
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
            // Mirror the input_tokens fallback for cache counters.
            if let Some(t) = data["usage"]["cache_read_input_tokens"].as_u64() {
                if t > 0 {
                    state.cache_read_tokens = t as u32;
                }
            }
            if let Some(t) = data["usage"]["cache_creation_input_tokens"].as_u64() {
                if t > 0 {
                    state.cache_write_tokens = t as u32;
                }
            }
            vec![
                StreamEvent::Usage(TokenUsage {
                    input_tokens: state.input_tokens,
                    output_tokens,
                    cache_read_tokens: state.cache_read_tokens,
                    cache_write_tokens: state.cache_write_tokens,
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

    #[test]
    fn provider_normalized_manifest_ignores_rolling_marker_and_keeps_system_stable() {
        let provider = AnthropicProvider::new("test-key", "claude-sonnet-4-6");
        let config = ChatConfig {
            prompt_cache_context: Some(crate::PromptCacheContext {
                affinity_key: "unused".to_owned(),
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
        let first_messages = vec![Message::system("stable"), Message::user("first")];
        let mut next_messages = first_messages.clone();
        next_messages.push(Message::assistant("answer"));
        next_messages.push(Message::user("next"));

        let first_request = provider.build_request(&first_messages, &tools, &config);
        let next_request = provider.build_request(&next_messages, &tools, &config);
        let first = provider.prompt_cache_input_manifest(&first_request, &config);
        let next = provider.prompt_cache_input_manifest(&next_request, &config);
        let comparison = first.compare_prefix(&next);

        assert_eq!(first.stable_prefix_hash, next.stable_prefix_hash);
        assert_eq!(comparison.conversation_prefix_segments, 1);
        assert_eq!(comparison.invalidation_reason, None);
        assert!(comparison.reusable_normalized_bytes > 0);
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
        // (Caching off: this test pins the legacy plain-string content shape;
        // breakpoint placement has its own tests below.)
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(false);
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
        // `content: ""` which Anthropic rejects. (Caching off: pins the
        // legacy plain-string content shape.)
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(false);
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
        // (Caching off: pins the legacy plain-string content shape.)
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(false);

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
        // text (pre-fix behaviour) for that row only. (Caching off: pins the
        // legacy plain-string content shape.)
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(false);
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
        // Pin caching ON: the extraction assertion below reads block-form
        // `system[0].text`, which only exists when caching is enabled — keep
        // it hermetic w.r.t. an ambient `OCTOS_PROMPT_CACHING=0`.
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(true);
        let messages = vec![
            msg(MessageRole::System, "system prompt"),
            msg(MessageRole::User, "hello"),
            msg(MessageRole::Assistant, "hi"),
        ];
        let config = ChatConfig::default();
        let request = provider.build_request(&messages, &[], &config);

        // System message should be extracted, not in messages array
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(
            body["system"][0]["text"], "system prompt",
            "system row extracted into the top-level field: {body}"
        );
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
    fn should_omit_thinking_when_reasoning_is_disabled() {
        let provider = AnthropicProvider::new("test-key", "claude-test");
        let messages = vec![msg(MessageRole::User, "hi")];
        let effort = serde_json::from_value(serde_json::json!("none"))
            .expect("none should disable reasoning");
        let config = ChatConfig {
            reasoning_effort: Some(effort),
            ..Default::default()
        };

        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        assert!(body.get("thinking").is_none());
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

    /// `tool_choice: {"type": "none"}` must reach the wire for an explicit
    /// choice and stay absent for the default, so ordinary requests are
    /// byte-identical to before. Anthropic invalidates message-level cache
    /// entries when it changes, so it is also a stable manifest segment.
    #[test]
    fn should_serialize_tool_choice_none_and_record_it_as_a_manifest_segment() {
        let provider = AnthropicProvider::new("key", "model");
        let messages = [
            msg(MessageRole::System, "system"),
            msg(MessageRole::User, "hello"),
        ];
        let tools = [tool_spec("read", "read a file")];
        let auto =
            serde_json::to_value(provider.build_request(&messages, &tools, &ChatConfig::default()))
                .unwrap();
        assert!(auto.get("tool_choice").is_none(), "{auto}");

        let none = ChatConfig {
            tool_choice: crate::ToolChoice::None,
            ..Default::default()
        };
        let request = provider.build_request(&messages, &tools, &none);
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["tool_choice"], serde_json::json!({"type": "none"}));
        let manifest = provider.prompt_cache_input_manifest(&request, &none);
        assert!(
            manifest
                .stable_segments
                .iter()
                .any(|segment| segment.kind == "config:tool_choice"),
            "tool_choice changes Anthropic's message cache and must be visible in the manifest"
        );
        let tool_less =
            serde_json::to_value(provider.build_request(&messages, &[], &none)).unwrap();
        assert!(tool_less.get("tool_choice").is_none(), "{tool_less}");
    }

    #[test]
    fn custom_compatible_endpoint_omits_cache_control_by_default() {
        let provider =
            AnthropicProvider::new("key", "model").with_base_url("https://custom.api.com");
        let body = serde_json::to_value(provider.build_request(
            &[
                msg(MessageRole::System, "system"),
                msg(MessageRole::User, "hello"),
            ],
            &[tool_spec("read", "read a file")],
            &ChatConfig::default(),
        ))
        .unwrap();

        assert_eq!(body["system"], "system");
        assert!(body["messages"][0]["content"].is_string(), "{body}");
        assert!(
            !body.to_string().contains("cache_control"),
            "custom endpoints must opt in to Anthropic cache extensions: {body}"
        );
    }

    #[test]
    fn explicit_prompt_caching_opt_in_wins_for_custom_endpoint_in_either_order() {
        let custom_then_opt_in = AnthropicProvider::new("key", "model")
            .with_base_url("https://custom.api.com")
            .with_prompt_caching(true);
        let opt_in_then_custom = AnthropicProvider::new("key", "model")
            .with_prompt_caching(true)
            .with_base_url("https://custom.api.com");

        for provider in [custom_then_opt_in, opt_in_then_custom] {
            let body = serde_json::to_value(provider.build_request(
                &[msg(MessageRole::User, "hello")],
                &[],
                &ChatConfig::default(),
            ))
            .unwrap();
            assert!(
                body.to_string().contains("cache_control"),
                "an explicit opt-in must survive builder call ordering: {body}"
            );
        }
    }

    #[test]
    fn endpoint_prompt_cache_defaults_are_official_only_and_honor_kill_switch() {
        assert!(prompt_caching_default_for_base_url_from(
            "https://api.anthropic.com",
            None
        ));
        assert!(prompt_caching_default_for_base_url_from(
            " https://API.ANTHROPIC.COM/ ",
            Some("true")
        ));
        assert!(!prompt_caching_default_for_base_url_from(
            "https://api.anthropic.com",
            Some("off")
        ));
        assert!(!prompt_caching_default_for_base_url_from(
            "https://custom.api.com",
            None
        ));
        assert!(!prompt_caching_default_for_base_url_from(
            "https://custom.api.com",
            Some("true")
        ));
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

    // --- prompt caching tests ---
    //
    // Anthropic bills the full replayed conversation at the full input rate
    // unless the request marks cache breakpoints. The provider emits three
    // `cache_control: {"type": "ephemeral"}` breakpoints (Anthropic allows
    // up to 4): the system prompt block, the LAST tool definition, and the
    // last content block of the LAST user-role message — stable prefix
    // (tools + system) plus rolling conversation history.

    fn tool_spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn should_mark_system_last_tool_and_last_user_block_with_cache_control() {
        // Pin caching ON so this wire-shape assertion is hermetic w.r.t. an
        // ambient `OCTOS_PROMPT_CACHING=0` (the builder override wins over the
        // env default). The default-resolution truth table is covered
        // separately by `prompt_caching_env_*` unit tests.
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(true);
        let tools = vec![
            tool_spec("alpha", "first tool"),
            tool_spec("omega", "last tool"),
        ];
        let messages = vec![
            msg(MessageRole::System, "system prompt"),
            msg(MessageRole::User, "first question"),
            msg(MessageRole::Assistant, "first answer"),
            msg(MessageRole::User, "second question"),
        ];
        let config = ChatConfig::default();
        let body =
            serde_json::to_value(provider.build_request(&messages, &tools, &config)).unwrap();

        // (a) System prompt: block form with the ephemeral marker. Anthropic
        // accepts both string and block-array `system`; only the block form
        // can carry cache_control.
        let system = body["system"]
            .as_array()
            .unwrap_or_else(|| panic!("system must be a block array: {body}"));
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "system prompt");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");

        // (b) ONLY the last tool carries cache_control (a breakpoint caches
        // everything before it, so one marker on the last tool covers the
        // whole tool array).
        let tools_out = body["tools"].as_array().unwrap();
        assert_eq!(tools_out.len(), 2);
        assert!(
            tools_out[0].get("cache_control").is_none(),
            "only the LAST tool may carry cache_control: {body}"
        );
        assert_eq!(tools_out[1]["name"], "omega");
        assert_eq!(tools_out[1]["description"], "last tool");
        assert_eq!(tools_out[1]["input_schema"]["type"], "object");
        assert_eq!(tools_out[1]["cache_control"]["type"], "ephemeral");

        // (c) ONLY the last user message's last content block carries
        // cache_control; earlier messages keep the plain-string shape.
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert!(
            msgs[0]["content"].is_string(),
            "earlier user message must stay plain text: {body}"
        );
        assert!(
            msgs[1]["content"].is_string(),
            "assistant message must stay plain text: {body}"
        );
        let blocks = msgs[2]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("last user content must be blocks: {body}"));
        let last_block = blocks.last().unwrap();
        assert_eq!(last_block["type"], "text");
        assert_eq!(last_block["text"], "second question");
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn should_place_message_breakpoint_on_last_tool_result_block() {
        // Mid tool-loop the last user-role message is the merged tool_result
        // batch — the breakpoint must land on its LAST block only, so the
        // whole history including the current results is cached for the
        // next iteration.
        // Pin caching ON so the assertion is independent of the ambient
        // `OCTOS_PROMPT_CACHING` kill-switch (builder override wins).
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(true);
        let mut assistant = msg(MessageRole::Assistant, "");
        assistant.tool_calls = Some(vec![
            tool_call("toolu_a", "shell", serde_json::json!({"command": "ls"})),
            tool_call("toolu_b", "read_file", serde_json::json!({"path": "x"})),
        ]);
        let mut result_a = msg(MessageRole::Tool, "out-a");
        result_a.tool_call_id = Some("toolu_a".into());
        let mut result_b = msg(MessageRole::Tool, "out-b");
        result_b.tool_call_id = Some("toolu_b".into());
        let messages = vec![msg(MessageRole::User, "go"), assistant, result_a, result_b];

        let config = ChatConfig::default();
        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        let results = msgs[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results[0].get("cache_control").is_none(),
            "only the LAST tool_result block may carry cache_control: {body}"
        );
        assert_eq!(results[1]["type"], "tool_result");
        assert_eq!(results[1]["tool_use_id"], "toolu_b");
        assert_eq!(results[1]["cache_control"]["type"], "ephemeral");
        // The plain user message BEFORE the tool loop must stay untouched.
        assert!(msgs[0]["content"].is_string(), "{body}");
    }

    #[test]
    fn should_not_mark_an_incomplete_parallel_tool_result_batch() {
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(true);
        let mut assistant = msg(MessageRole::Assistant, "");
        assistant.tool_calls = Some(vec![
            tool_call("toolu_a", "shell", serde_json::json!({"command": "ls"})),
            tool_call("toolu_b", "read_file", serde_json::json!({"path": "x"})),
        ]);
        let mut result_a = msg(MessageRole::Tool, "out-a");
        result_a.tool_call_id = Some("toolu_a".into());
        let messages = vec![msg(MessageRole::User, "go"), assistant, result_a];

        let body =
            serde_json::to_value(provider.build_request(&messages, &[], &ChatConfig::default()))
                .unwrap();
        let messages = body["messages"].as_array().unwrap();
        let first_user = messages[0]["content"].as_array().unwrap();
        assert_eq!(
            first_user.last().unwrap()["cache_control"]["type"],
            "ephemeral",
            "the prior complete user turn remains the rolling breakpoint: {body}"
        );
        assert!(
            !messages[2].to_string().contains("cache_control"),
            "an incomplete tool-result batch must not become a semantic cache boundary: {body}"
        );
    }

    #[test]
    fn should_keep_blank_system_as_string_even_when_caching_enabled() {
        // An empty text BLOCK is rejected by Anthropic while `"system": ""`
        // is not — an all-blank system prompt must stay in string form.
        // Pin caching ON (the test name asserts "even_when_caching_enabled")
        // so it does not silently pass under an ambient `OCTOS_PROMPT_CACHING=0`.
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(true);
        let messages = vec![msg(MessageRole::System, ""), msg(MessageRole::User, "hi")];
        let config = ChatConfig::default();
        let body = serde_json::to_value(provider.build_request(&messages, &[], &config)).unwrap();
        assert_eq!(
            body["system"], "",
            "blank system must not become an empty block: {body}"
        );
    }

    #[test]
    fn should_not_emit_cache_control_anywhere_when_prompt_caching_disabled() {
        // `with_prompt_caching(false)` must restore the exact pre-caching
        // wire shape for odd Anthropic-compatible proxies: plain-string
        // system, verbatim tools, no cache_control key anywhere.
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(false);
        let tools = vec![
            tool_spec("alpha", "first tool"),
            tool_spec("omega", "last tool"),
        ];
        let messages = vec![
            msg(MessageRole::System, "system prompt"),
            msg(MessageRole::User, "hello"),
        ];
        let config = ChatConfig::default();
        let body =
            serde_json::to_value(provider.build_request(&messages, &tools, &config)).unwrap();

        assert_eq!(
            body["system"], "system prompt",
            "system must stay a plain string when caching is off: {body}"
        );
        assert!(
            body["messages"][0]["content"].is_string(),
            "user content must stay a plain string when caching is off: {body}"
        );
        assert!(
            !body.to_string().contains("cache_control"),
            "no cache_control key may appear anywhere when caching is off: {body}"
        );
    }

    #[test]
    fn should_not_emit_cache_control_anywhere_when_request_opts_out_of_cache_writes() {
        // A one-shot request (`ChatConfig.cache_retention: None`) must not
        // pay the 1.25x cache-write premium: with caching enabled on the
        // PROVIDER, the opted-out REQUEST still serializes to the exact
        // pre-caching wire shape — plain-string system, verbatim tools, no
        // cache_control key anywhere.
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(true);
        let tools = vec![
            tool_spec("alpha", "first tool"),
            tool_spec("omega", "last tool"),
        ];
        let messages = vec![
            msg(MessageRole::System, "system prompt"),
            msg(MessageRole::User, "hello"),
        ];
        let opted_out = ChatConfig {
            cache_retention: crate::CacheRetention::None,
            ..Default::default()
        };
        let body =
            serde_json::to_string(&provider.build_request(&messages, &tools, &opted_out)).unwrap();
        assert!(
            !body.contains("cache_control"),
            "an opted-out request must carry zero cache_control blocks: {body}"
        );

        // Byte-identical to the shape a caching-disabled provider emits —
        // the opt-out and the provider-level kill switch are the same wire
        // contract.
        let disabled = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(false);
        let disabled_body = serde_json::to_string(&disabled.build_request(
            &messages,
            &tools,
            &ChatConfig::default(),
        ))
        .unwrap();
        assert_eq!(
            body, disabled_body,
            "opted-out request must match the caching-disabled wire shape byte-for-byte"
        );
    }

    #[test]
    fn should_keep_default_request_byte_identical_when_cache_retention_unset() {
        // The opt-out is strictly per-request: a config that never touches
        // `cache_retention` (and one that sets it to `Default` explicitly)
        // must keep the exact cached wire shape, breakpoints included.
        let provider = AnthropicProvider::new("test-key", "claude-test").with_prompt_caching(true);
        let tools = vec![tool_spec("alpha", "first tool")];
        let messages = vec![
            msg(MessageRole::System, "system prompt"),
            msg(MessageRole::User, "hello"),
        ];
        let unset = serde_json::to_string(&provider.build_request(
            &messages,
            &tools,
            &ChatConfig::default(),
        ))
        .unwrap();
        let explicit_default = ChatConfig {
            cache_retention: crate::CacheRetention::Default,
            ..Default::default()
        };
        let explicit =
            serde_json::to_string(&provider.build_request(&messages, &tools, &explicit_default))
                .unwrap();
        assert_eq!(unset, explicit);
        assert!(
            unset.contains("cache_control"),
            "the default request must keep its cache breakpoints: {unset}"
        );
    }

    #[test]
    fn prompt_caching_env_default_stays_on_when_unset_or_truthy() {
        // Default ON is preserved: unset, empty, or any non-falsy value keeps
        // caching enabled (Claude Code sends cache_control unconditionally).
        assert!(prompt_caching_default_from(None));
        assert!(prompt_caching_default_from(Some("")));
        assert!(prompt_caching_default_from(Some("1")));
        assert!(prompt_caching_default_from(Some("true")));
        assert!(prompt_caching_default_from(Some("on")));
        assert!(prompt_caching_default_from(Some("yes")));
        assert!(prompt_caching_default_from(Some("anything-else")));
    }

    #[test]
    fn prompt_caching_env_kill_switch_disables_on_falsy_values() {
        // OCTOS_PROMPT_CACHING kill-switch: disable without a rebuild for any
        // Anthropic-compatible proxy that rejects cache_control / block-form
        // system. Case- and whitespace-insensitive.
        for v in ["0", "false", "FALSE", "off", "Off", "no", "  no ", " 0 "] {
            assert!(
                !prompt_caching_default_from(Some(v)),
                "OCTOS_PROMPT_CACHING={v:?} must disable prompt caching"
            );
        }
    }

    #[test]
    fn should_parse_cache_usage_fields_from_response() {
        let api_response: AnthropicResponse = serde_json::from_value(serde_json::json!({
            "content": [{ "type": "text", "text": "hi" }],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 3,
                "output_tokens": 7,
                "cache_creation_input_tokens": 4096,
                "cache_read_input_tokens": 10240
            }
        }))
        .unwrap();

        let response = anthropic_response_to_chat_response(api_response);
        assert_eq!(response.usage.input_tokens, 3);
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(response.usage.cache_write_tokens, 4096);
        assert_eq!(response.usage.cache_read_tokens, 10240);
    }

    #[test]
    fn should_emit_cache_usage_in_stream_events() {
        let mut state = AnthropicStreamState::default();
        let start = crate::sse::SseEvent {
            event: None,
            data: r#"{"type":"message_start","message":{"usage":{"input_tokens":3,"cache_creation_input_tokens":4096,"cache_read_input_tokens":10240}}}"#.into(),
        };
        assert!(map_anthropic_sse(&mut state, &start).is_empty());

        let end = crate::sse::SseEvent {
            event: None,
            data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#.into(),
        };
        let events = map_anthropic_sse(&mut state, &end);
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 3);
                assert_eq!(u.output_tokens, 9);
                assert_eq!(u.cache_write_tokens, 4096);
                assert_eq!(u.cache_read_tokens, 10240);
            }
            other => panic!("expected Usage event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_send_cache_breakpoints_and_parse_cache_usage_end_to_end() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(
                        r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":2048}}"#,
                    )
                    .append_header("Content-Type", "application/json"),
            )
            .mount(&server)
            .await;

        // Pin caching ON — this test asserts cache breakpoints are sent on the
        // wire, so it must not depend on the ambient `OCTOS_PROMPT_CACHING`.
        let provider = AnthropicProvider::new("test-key", "claude-test")
            .with_base_url(server.uri())
            .with_prompt_caching(true);
        let tools = vec![tool_spec("shell", "run a command")];
        let messages = vec![
            msg(MessageRole::System, "sys"),
            msg(MessageRole::User, "hi"),
        ];
        let response = provider
            .chat(&messages, &tools, &ChatConfig::default())
            .await
            .unwrap();
        assert_eq!(response.usage.cache_read_tokens, 2048);
        assert_eq!(response.usage.cache_write_tokens, 0);

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        let tools_out = body["tools"].as_array().unwrap();
        assert_eq!(
            tools_out.last().unwrap()["cache_control"]["type"],
            "ephemeral"
        );
        let msgs = body["messages"].as_array().unwrap();
        let blocks = msgs.last().unwrap()["content"].as_array().unwrap();
        assert_eq!(blocks.last().unwrap()["cache_control"]["type"], "ephemeral");
    }
    // --- sampling params on the Anthropic protocol path (#2172) ---
    //
    // Emission is MODEL-CAPABILITY-AWARE: first-party Claude (claude-*, incl.
    // Opus 4.7 which we ship) rejects sampling on the Messages API and must
    // receive NONE (reverting to the pre-#2172 wire); GLM via z.ai accepts
    // temperature/top_p/top_k. The `model_accepts_sampling` gate below is
    // pinned from both sides so a "forward to all" mutation fails the Claude
    // tests and a "forward to none" mutation fails the GLM tests.

    /// Pre-change golden serialization of a representative request (caching
    /// ON: system + tool + user message, `ChatConfig::default()`), captured
    /// on the rev before sampling support was added. The no-override wire
    /// must stay byte-identical — prompt-cache prefixes and Anthropic-
    /// compatible proxies depend on the exact shape.
    const NO_OVERRIDE_GOLDEN: &str = r#"{"model":"claude-test","max_tokens":16384,"messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]}],"system":[{"type":"text","text":"system prompt","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"alpha","description":"first tool","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}]}"#;

    fn fixture_for(model: &str) -> (AnthropicProvider, Vec<ToolSpec>, Vec<Message>) {
        // Pin caching ON so the wire shape is hermetic w.r.t. an ambient
        // `OCTOS_PROMPT_CACHING=0` (builder override wins over the env
        // default), and so the goldens cover cache_control placement.
        let provider = AnthropicProvider::new("test-key", model).with_prompt_caching(true);
        let tools = vec![tool_spec("alpha", "first tool")];
        let messages = vec![
            msg(MessageRole::System, "system prompt"),
            msg(MessageRole::User, "hello"),
        ];
        (provider, tools, messages)
    }

    /// First-party Claude, rejects sampling. `claude-opus-4-7` is a real
    /// `model_catalog.json` entry whose API contract removed the sampler set.
    fn claude_fixture() -> (AnthropicProvider, Vec<ToolSpec>, Vec<Message>) {
        fixture_for("claude-opus-4-7")
    }

    /// GLM via z.ai (the `zai` / `zai-coding` families), accepts sampling.
    fn glm_fixture() -> (AnthropicProvider, Vec<ToolSpec>, Vec<Message>) {
        fixture_for("glm-5.3")
    }

    #[test]
    fn should_accept_only_glm_and_reject_everything_else() {
        // The capability gate is DEFAULT-DENY: GLM via z.ai is the sole
        // affirmatively-accepting class; everything else — first-party Claude
        // (bare AND family-qualified), other OpenAI-path models that could be
        // pointed here, custom endpoints, and the empty string — gets
        // nothing (safe direction; also matches origin/main, which forwarded
        // no sampling to any Anthropic-path model). Pinned so a forward-to-all
        // or forward-to-none regression is caught here before the
        // request-shape tests.
        for accept in [
            // every GLM suffix the zai / zai-coding constructors can send
            // (bare model, after the registry strips the `<family>/` prefix)
            "glm-4.5-air",
            "glm-4.7",
            "glm-5-turbo",
            "glm-5.1",
            "glm-5.3",
            "glm-5.3-flash",
            "GLM-5.3",        // case-insensitive
            "zai/glm-4.7",    // family-qualified (custom base_url)
            "vendor/glm-5.3", // any leading path segment
        ] {
            assert!(
                model_accepts_sampling(accept),
                "{accept} must be classified as ACCEPTING sampling"
            );
        }
        for reject in [
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-3-5-haiku-20241022",
            "claude-test",
            "Claude-Opus-4-7",           // case-insensitive
            "anthropic/claude-opus-4-7", // family-qualified claude (H1 bypass)
            "r9s/claude-opus-4-7",       // proxied claude (H1 bypass)
            "kimi-k2",                   // OpenAI-path model, not GLM
            "minimax-m2.1",
            "my-local-model", // unknown custom endpoint
            "glmini",         // "glm" without the hyphen is not GLM
            "",               // empty string
        ] {
            assert!(
                !model_accepts_sampling(reject),
                "{reject} must be classified as REJECTING sampling"
            );
        }
    }

    #[test]
    fn should_serialize_byte_identical_golden_when_no_sampling_override_configured() {
        let (provider, tools, messages) = fixture_for("claude-test");
        // The agent loop's no-override shape: `ChatConfig::default()` carries
        // the built-in `temperature: Some(0.0)` sentinel (see the #2172
        // invariant in octos-agent's `build_chat_config`) and no
        // sampling_params.
        let config = ChatConfig::default();
        let wire =
            serde_json::to_string(&provider.build_request(&messages, &tools, &config)).unwrap();
        assert_eq!(wire, NO_OVERRIDE_GOLDEN);

        // An explicitly-unset temperature must produce the very same bytes.
        let config_none = ChatConfig {
            temperature: None,
            ..ChatConfig::default()
        };
        let wire_none =
            serde_json::to_string(&provider.build_request(&messages, &tools, &config_none))
                .unwrap();
        assert_eq!(wire_none, NO_OVERRIDE_GOLDEN);
    }

    #[test]
    fn should_emit_no_sampling_fields_on_accepting_model_when_no_override_configured() {
        // Byte-identity's sibling on the ACCEPTING path: even a GLM model
        // adds nothing when the operator configured no override (0.0
        // sentinel, no sampling_params) — so cloud GLM requests are unchanged
        // until an operator opts in.
        let (provider, tools, messages) = glm_fixture();
        let body =
            serde_json::to_value(provider.build_request(&messages, &tools, &ChatConfig::default()))
                .unwrap();
        assert!(body.get("temperature").is_none(), "{body}");
        assert!(body.get("top_p").is_none(), "{body}");
        assert!(body.get("top_k").is_none(), "{body}");
    }

    #[test]
    fn should_forward_temperature_top_p_and_top_k_to_glm_when_operator_overrides() {
        let (provider, tools, messages) = glm_fixture();
        let mut sp = serde_json::Map::new();
        sp.insert("top_p".to_string(), serde_json::json!(0.9));
        sp.insert("top_k".to_string(), serde_json::json!(40));
        let config = ChatConfig {
            // 0.5 is exactly representable in f32, so the f32 -> f64 widening
            // in `serde_json::to_value` cannot skew the equality check.
            temperature: Some(0.5),
            sampling_params: Some(sp),
            ..ChatConfig::default()
        };
        let body =
            serde_json::to_value(provider.build_request(&messages, &tools, &config)).unwrap();
        assert_eq!(body["temperature"], serde_json::json!(0.5), "{body}");
        assert_eq!(body["top_p"], serde_json::json!(0.9), "{body}");
        assert_eq!(body["top_k"], serde_json::json!(40), "{body}");
    }

    #[test]
    fn should_serialize_temperature_shortest_form_on_glm_wire() {
        // The non-streaming path serializes the f32 directly (ryu shortest),
        // so 0.7 reaches the wire as `0.7`, not the widened `0.699999…`.
        let (provider, tools, messages) = glm_fixture();
        let config = ChatConfig {
            temperature: Some(0.7),
            ..ChatConfig::default()
        };
        let wire =
            serde_json::to_string(&provider.build_request(&messages, &tools, &config)).unwrap();
        assert!(
            wire.contains("\"temperature\":0.7"),
            "temperature override must reach the wire with its exact value: {wire}"
        );
    }

    #[test]
    fn should_treat_zero_temperature_as_unset_sentinel_even_on_accepting_model() {
        // 0.0 is the plumbing's built-in default (#2172); it is
        // indistinguishable from "unset" and must never be emitted, even to a
        // model that WOULD accept a real temperature.
        let (provider, tools, messages) = glm_fixture();
        let config = ChatConfig {
            temperature: Some(0.0),
            ..ChatConfig::default()
        };
        let body =
            serde_json::to_value(provider.build_request(&messages, &tools, &config)).unwrap();
        assert!(
            body.get("temperature").is_none(),
            "the 0.0 default sentinel must stay off the wire: {body}"
        );
    }

    #[test]
    fn should_forward_top_p_top_k_and_drop_openai_only_keys_on_glm() {
        let (provider, tools, messages) = glm_fixture();
        let mut sp = serde_json::Map::new();
        sp.insert("top_p".to_string(), serde_json::json!(0.95));
        sp.insert("top_k".to_string(), serde_json::json!(40));
        sp.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
        sp.insert("frequency_penalty".to_string(), serde_json::json!(0.5));
        let config = ChatConfig {
            sampling_params: Some(sp),
            ..ChatConfig::default()
        };
        let body =
            serde_json::to_value(provider.build_request(&messages, &tools, &config)).unwrap();
        assert_eq!(body["top_p"], serde_json::json!(0.95), "{body}");
        assert_eq!(body["top_k"], serde_json::json!(40), "{body}");
        // OpenAI-only sampler knobs are NOT part of the Anthropic Messages
        // API — they must be dropped (and logged), never forwarded verbatim.
        assert!(body.get("repeat_penalty").is_none(), "{body}");
        assert!(body.get("frequency_penalty").is_none(), "{body}");
    }

    #[test]
    fn should_drop_modeled_keys_when_smuggled_via_sampling_params_on_glm() {
        // Defense-in-depth, mirroring the OpenAI path (#2172): keys octos
        // models with dedicated fields cannot sneak in through
        // sampling_params and emit duplicate/divergent top-level keys.
        let (provider, tools, messages) = glm_fixture();
        let mut sp = serde_json::Map::new();
        sp.insert("temperature".to_string(), serde_json::json!(1.9));
        sp.insert("max_tokens".to_string(), serde_json::json!(9));
        let config = ChatConfig {
            sampling_params: Some(sp),
            ..ChatConfig::default()
        };
        let body =
            serde_json::to_value(provider.build_request(&messages, &tools, &config)).unwrap();
        assert!(
            body.get("temperature").is_none(),
            "smuggled temperature must not reach the wire: {body}"
        );
        assert_eq!(
            body["max_tokens"],
            serde_json::json!(crate::context::default_max_tokens()),
            "dedicated max_tokens field must win: {body}"
        );
    }

    #[test]
    fn should_keep_cache_breakpoints_unchanged_when_sampling_forwarded_to_glm() {
        // #1640 interaction: sampling fields are top-level request fields and
        // must not disturb cache_control placement (system block, LAST tool,
        // last user content block — exactly three markers). Exercised on the
        // ACCEPTING path so the sampling fields are actually present.
        let (provider, tools, messages) = glm_fixture();
        let mut sp = serde_json::Map::new();
        sp.insert("top_p".to_string(), serde_json::json!(0.9));
        let config = ChatConfig {
            temperature: Some(0.7),
            sampling_params: Some(sp),
            ..ChatConfig::default()
        };
        let wire =
            serde_json::to_string(&provider.build_request(&messages, &tools, &config)).unwrap();
        assert_eq!(
            wire.matches("\"cache_control\"").count(),
            3,
            "exactly three breakpoints (system, last tool, last user block): {wire}"
        );
        let body: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            body["tools"].as_array().unwrap().last().unwrap()["cache_control"]["type"],
            "ephemeral"
        );
        let blocks = body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_array()
            .unwrap();
        assert_eq!(blocks.last().unwrap()["cache_control"]["type"], "ephemeral");
        assert!(body.get("temperature").is_some(), "{body}");
        assert_eq!(body["top_p"], serde_json::json!(0.9), "{body}");
    }

    #[test]
    fn should_not_send_any_sampling_to_first_party_claude_when_operator_overrides() {
        // H3 core regression: without the model gate, an operator temperature
        // (and any sampling_params) reach first-party Claude and 400 on
        // Opus 4.7+. No thinking here — this must hold on the plain path too.
        let (provider, tools, messages) = claude_fixture();
        let mut sp = serde_json::Map::new();
        sp.insert("top_p".to_string(), serde_json::json!(0.9));
        sp.insert("top_k".to_string(), serde_json::json!(40));
        let config = ChatConfig {
            temperature: Some(0.7),
            sampling_params: Some(sp),
            ..ChatConfig::default()
        };
        let body =
            serde_json::to_value(provider.build_request(&messages, &tools, &config)).unwrap();
        assert!(body.get("temperature").is_none(), "{body}");
        assert!(body.get("top_p").is_none(), "{body}");
        assert!(body.get("top_k").is_none(), "{body}");
    }

    #[test]
    fn should_not_send_any_sampling_to_opus_4_7_when_thinking_omitted_for_small_max_tokens() {
        // H3 explicit: the small-max_tokens path drops `thinking` (no valid
        // budget fits), which is exactly where the first cut leaked ALL
        // sampling to Opus 4.7. The model gate must still suppress everything.
        let (provider, tools, messages) = claude_fixture();
        let mut sp = serde_json::Map::new();
        sp.insert("top_p".to_string(), serde_json::json!(0.9));
        let config = ChatConfig {
            max_tokens: Some(1_000),
            temperature: Some(0.7),
            reasoning_effort: Some(ReasoningEffort::Low),
            sampling_params: Some(sp),
            ..ChatConfig::default()
        };
        let body =
            serde_json::to_value(provider.build_request(&messages, &tools, &config)).unwrap();
        assert!(body.get("thinking").is_none(), "budget cannot fit: {body}");
        assert!(body.get("temperature").is_none(), "{body}");
        assert!(body.get("top_p").is_none(), "{body}");
        assert!(body.get("top_k").is_none(), "{body}");
    }

    #[test]
    fn should_not_send_top_p_to_first_party_claude_even_when_thinking_enabled() {
        // H4: `thinking + top_p` 400s first-party Claude (top_p is only
        // conditionally allowed under thinking, and not at all on 4.7+). The
        // gate drops top_p regardless of the thinking block.
        let (provider, tools, messages) = claude_fixture();
        let mut sp = serde_json::Map::new();
        sp.insert("top_p".to_string(), serde_json::json!(0.9));
        let config = ChatConfig {
            max_tokens: Some(32_768),
            reasoning_effort: Some(ReasoningEffort::High),
            sampling_params: Some(sp),
            ..ChatConfig::default()
        };
        let body =
            serde_json::to_value(provider.build_request(&messages, &tools, &config)).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled", "{body}");
        assert!(body.get("top_p").is_none(), "{body}");
    }
}
