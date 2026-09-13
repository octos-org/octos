//! LLM provider trait.

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;

use crate::config::ChatConfig;
use crate::context;
use crate::types::{ChatResponse, ChatStream, ProviderMetadata, StreamEvent, ToolSpec};

/// Trait for LLM providers.
///
/// This is intentionally minimal to reduce abstraction overhead.
/// Each provider implements the specifics of its API.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Send a chat completion request.
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse>;

    /// Stream a chat completion response.
    /// Default: falls back to non-streaming chat() and emits events.
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let response = self.chat(messages, tools, config).await?;
        let mut events: Vec<StreamEvent> = Vec::new();
        if let Some(text) = response.content.clone() {
            events.push(StreamEvent::TextDelta(text));
        }
        for (i, tc) in response.tool_calls.iter().enumerate() {
            events.push(StreamEvent::ToolCallDelta {
                index: i,
                id: Some(tc.id.clone()),
                name: Some(tc.name.clone()),
                arguments_delta: tc.arguments.to_string(),
            });
        }
        events.push(StreamEvent::Usage(response.usage));
        events.push(StreamEvent::Done(response.stop_reason));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    /// Get the context window size in tokens for this model.
    fn context_window(&self) -> u32 {
        context::context_window_tokens(self.model_id())
    }

    /// Complete any asynchronous initialization that feeds the SYNC
    /// accessors (notably [`Self::context_window`]). Default: no-op.
    ///
    /// The agent loop and the prompt-context bridges await this before
    /// their first window-dependent decision of a turn, so a provider that
    /// learns its true window asynchronously — the local context probe —
    /// resolves BEFORE compaction reads the window, not after the first
    /// chat. Implementations must return immediately once resolved and be
    /// bounded (a few seconds at most) when not. Wrappers delegate.
    async fn ensure_ready(&self) {}

    /// Get the maximum output tokens this model supports per call.
    fn max_output_tokens(&self) -> u32 {
        context::max_output_tokens(self.model_id())
    }

    /// #2143 part 3: estimate the token size of the REQUEST this provider would
    /// build from `messages` + `tools`, INCLUDING provider-specific
    /// serialization overhead (a separate system block, per-message
    /// content-block framing, cache-control metadata) that the flat estimator
    /// omits. The route-fit guard calls this instead of summing messages/tools
    /// itself, so the ~12.5% safety margin no longer has to stand in for that
    /// overhead. The default is the provider-agnostic base estimate; concrete
    /// providers override to tighten it, and WRAPPERS DELEGATE to their inner
    /// provider so the override survives the RetryProvider / context-override /
    /// local-probe stack the router dispatches through.
    fn estimate_request_tokens(
        &self,
        messages: &[Message],
        tools: &[crate::types::ToolSpec],
    ) -> u32 {
        context::estimate_request_tokens_base(messages, tools)
    }

    /// Get the model identifier.
    fn model_id(&self) -> &str;

    /// Get the provider name (e.g., "anthropic", "openai").
    fn provider_name(&self) -> &str;

    /// Get structured metadata for the active provider instance.
    fn provider_metadata(&self) -> ProviderMetadata {
        ProviderMetadata::new(self.provider_name(), self.model_id(), None)
    }

    /// Get structured metadata for a concrete provider slot, when the caller
    /// knows which slot produced the response.
    ///
    /// `provider_index` is a FLAT index over the leaf lanes of the whole
    /// composition tree (see [`LlmProvider::provider_lane_count`]): a
    /// composite maps it onto its own slot by the slots' lane counts and
    /// forwards the remainder, so a chain inside a fallback (or the reverse)
    /// still resolves the lane that served the response.
    fn provider_metadata_for_index(&self, _provider_index: Option<usize>) -> ProviderMetadata {
        self.provider_metadata()
    }

    /// Number of concrete serving lanes reachable through this provider: 1
    /// for an adapter, the sum over its slots for a composite, the inner
    /// count for a wrapper. Together with `provider_metadata_for_index` it
    /// makes `ChatResponse.provider_index` (and the `StreamEvent::ProviderIndex`
    /// prefix) a flat index over the leaf lanes of the whole composition.
    fn provider_lane_count(&self) -> usize {
        1
    }

    /// Whether this runtime consumes semantic checkpoint hints from
    /// `ChatConfig.prompt_cache_context`. Hosted providers and ordinary
    /// OpenAI-compatible servers leave this false; experimental local/hybrid
    /// engines opt in explicitly. Transparent wrappers delegate to their
    /// concrete provider. A failover/hedging composite returns true when any
    /// reachable lane can consume the hints; unsupported concrete lanes still
    /// ignore this provider-neutral configuration rather than serializing it.
    fn supports_semantic_checkpoint_hints(&self) -> bool {
        false
    }

    /// Export provider QoS metrics as JSON (for adaptive routers).
    /// Returns `None` for simple providers; overridden by `AdaptiveRouter`.
    fn export_metrics(&self) -> Option<serde_json::Value> {
        None
    }

    /// Report a late failure (e.g. empty response detected after stream consumption).
    /// The adaptive router uses this to update failure metrics so subsequent calls
    /// may failover to a different provider.
    fn report_late_failure(&self) {}

    /// Report streaming throughput metrics after a stream is fully consumed.
    /// Used by the adaptive router to update throughput scoring.
    fn report_stream_metrics(&self, _output_tokens: u32, _stream_duration_us: u64) {}

    /// Wire protocol this provider speaks, rendered into lane-attributed
    /// transport errors and failover summaries (`api_style=…`). `None` for
    /// opaque third-party implementations; transparent wrappers delegate to
    /// their inner provider and composites report their current lane.
    fn api_style(&self) -> Option<ApiStyle> {
        None
    }
}

/// Wire protocol an adapter speaks. Rendered as `api_style=<name>` so a
/// failing lane is attributed to its configured provider label and protocol
/// instead of a hardcoded vendor name (a `zai-coding` lane speaks
/// `anthropic_messages` but is not "Anthropic").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiStyle {
    AnthropicMessages,
    OpenAiChatCompletions,
    OpenAiResponses,
    GeminiGenerateContent,
    OpenRouterChatCompletions,
}

impl ApiStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic_messages",
            Self::OpenAiChatCompletions => "openai_chat_completions",
            Self::OpenAiResponses => "openai_responses",
            Self::GeminiGenerateContent => "gemini_generate_content",
            Self::OpenRouterChatCompletions => "openrouter_chat_completions",
        }
    }
}

impl std::fmt::Display for ApiStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `provider/model (api_style=…)` — the lane identity used by transport
/// errors and failover summaries; `provider/model` when the style is unknown.
pub fn lane_label(provider: &str, model: &str, api_style: Option<ApiStyle>) -> String {
    match api_style {
        Some(style) => format!("{provider}/{model} (api_style={style})"),
        None => format!("{provider}/{model}"),
    }
}

/// Context for a request that never produced an HTTP response:
/// `failed to send [streaming] request to <lane_label>`. Every adapter uses
/// this so the message names the concrete lane the user configured.
pub fn transport_error_message(
    streaming: bool,
    provider: &str,
    model: &str,
    api_style: ApiStyle,
) -> String {
    let request = if streaming {
        "streaming request"
    } else {
        "request"
    };
    format!(
        "failed to send {request} to {}",
        lane_label(provider, model, Some(api_style))
    )
}

/// Operational stages an adapter can fail at once the request left the
/// builder; rendered by [`operational_error_message`] with the same lane
/// label as transport errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationalStage {
    SerializeRequest,
    BuildRequestBody,
    ReadResponseBody,
    ParseResponse,
    NoChoices,
    NoCandidates,
}

/// Uniform wording for adapter-side operational failures, naming the
/// concrete lane instead of a vendor, e.g.
/// `failed to parse response from zai-coding/glm-5.3 (api_style=anthropic_messages)`.
/// Official endpoints keep their vendor default label (`openai`, `anthropic`,
/// `gemini`, `openrouter`), so the vendor word appears exactly when the lane
/// really is that vendor.
pub fn operational_error_message(
    stage: OperationalStage,
    provider: &str,
    model: &str,
    api_style: ApiStyle,
) -> String {
    let lane = lane_label(provider, model, Some(api_style));
    match stage {
        OperationalStage::SerializeRequest => format!("failed to serialize request for {lane}"),
        OperationalStage::BuildRequestBody => format!("failed to build request body for {lane}"),
        OperationalStage::ReadResponseBody => format!("failed to read response body from {lane}"),
        OperationalStage::ParseResponse => format!("failed to parse response from {lane}"),
        OperationalStage::NoChoices => format!("no choices in response from {lane}"),
        OperationalStage::NoCandidates => format!("no candidates in response from {lane}"),
    }
}

/// One lane's failure captured by a composite provider before it moves on
/// (or gives up), so the final error names every lane that failed.
#[derive(Clone, Debug)]
pub struct LaneFailure {
    pub lane: String,
    pub error: String,
}

impl LaneFailure {
    /// Capture `error` as raised by `provider`, with the full cause chain
    /// (`{error:#}`) so the summary stays self-sufficient when only the
    /// top-level `Display` reaches the client.
    pub fn capture(provider: &dyn LlmProvider, error: &eyre::Report) -> Self {
        Self {
            lane: lane_label(
                provider.provider_name(),
                provider.model_id(),
                provider.api_style(),
            ),
            error: format!("{error:#}"),
        }
    }
}

/// Every eligible lane was tried.
pub(crate) const LANES_EXHAUSTED: &str = "all lanes failed";
/// The call policy forbade failover after the first failure.
pub(crate) const LANE_FAILED_FAIL_FAST: &str = "lane failed (fail-fast policy, no failover)";
/// The failure kind is not failover-worthy (`RetryProvider::should_failover`).
pub(crate) const LANE_FAILED_NOT_FAILOVER_WORTHY: &str =
    "lane failed (not failover-worthy, no failover)";

/// `<outcome>: <lane>: <error>; <lane>: <error>` — self-sufficient on its own.
pub fn lane_failure_summary(outcome: &str, failures: &[LaneFailure]) -> String {
    let rendered = failures
        .iter()
        .map(|failure| format!("{}: {}", failure.lane, failure.error))
        .collect::<Vec<_>>()
        .join("; ");
    format!("{outcome}: {rendered}")
}

/// Wrap `carrier` — the lane error whose typed cause chain must stay
/// classifiable (`RetryProvider::should_failover` / `is_retryable_error`
/// walk `chain()`) — with a summary naming every failed lane as the
/// top-level message: `{err}` shows the summary, `{err:#}` appends the
/// carrier's own chain.
pub fn attribute_lane_failures(
    carrier: eyre::Report,
    outcome: &str,
    failures: &[LaneFailure],
) -> eyre::Report {
    carrier.wrap_err(lane_failure_summary(outcome, failures))
}

pub(crate) fn endpoint_label_from_base_url(url: &str) -> Option<String> {
    let host = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()?
        .trim();
    if host.is_empty() {
        return None;
    }
    Some(host.trim_start_matches("www.").to_string())
}

/// Truncate an API error body to avoid leaking verbose internal details.
/// Keeps the first 200 chars which typically contain the error message/code.
pub(crate) fn truncate_error_body(body: &str) -> String {
    // Truncate on a char boundary, not a raw byte index: API error bodies are
    // often non-ASCII (e.g. Chinese providers like zhipu/minimax/moonshot), and
    // slicing at byte 200 mid-codepoint would panic.
    match body.char_indices().nth(200) {
        Some((cut, _)) => format!("{}... ({} bytes total)", &body[..cut], body.len()),
        None => body.to_string(),
    }
}

/// Default LLM request timeout in seconds, applied as a reqwest **total**
/// request deadline — for **non-streaming** requests only.
///
/// Streaming responses must NOT use a total timeout: it would cap a healthy,
/// actively-streaming generation regardless of progress (a slow local model
/// writing a large output gets cut off mid-stream). Streaming clients are
/// built by [`build_streaming_http_client`] instead, which bounds connect and
/// per-read stalls but never the total generation time. The tunable
/// stream-timeout system (TTFT / inter-chunk idle / overall wall-clock cap)
/// lives in `octos-agent`'s `streaming.rs` on the response body.
pub const DEFAULT_LLM_TIMEOUT_SECS: u64 = 300;
/// Default per-read idle timeout for **streaming** clients, in seconds.
///
/// Applied via reqwest's `.read_timeout()`, which **resets after every read**.
/// It bounds the initial header-wait and any genuine stall, but a stream that
/// keeps producing tokens is never capped, no matter how long the full
/// generation runs — mirroring Pi's `timeoutMs`-as-stream-idleness model.
pub const DEFAULT_LLM_STREAM_IDLE_TIMEOUT_SECS: u64 = 300;
/// Default LLM connect timeout in seconds.
/// Reduced from 30s: if a provider can't connect in 10s, fail over sooner.
pub const DEFAULT_LLM_CONNECT_TIMEOUT_SECS: u64 = 10;
/// Default embedding request timeout in seconds.
pub const DEFAULT_EMBEDDING_TIMEOUT_SECS: u64 = 60;
/// Default embedding connect timeout in seconds.
pub const DEFAULT_EMBEDDING_CONNECT_TIMEOUT_SECS: u64 = 15;

/// Build a `reqwest::Client` with a **total** request timeout.
/// The total timeout acts as a safety net for **non-streaming** requests —
/// individual callers can override with per-request `.timeout()` for tighter
/// or looser limits.
///
/// Do NOT use this for streaming: a total timeout caps the whole response and
/// kills a healthy, actively-streaming generation once it exceeds the deadline.
/// Use [`build_streaming_http_client`] for stream requests instead.
pub fn build_http_client(timeout_secs: u64, connect_timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("failed to build HTTP client")
}

/// Build a `reqwest::Client` for **streaming** requests.
///
/// Unlike [`build_http_client`], this sets **no total request timeout** — a
/// stream that keeps producing tokens must never be cut off, however long the
/// full generation runs. Instead it bounds stalls with `.read_timeout()`, which
/// applies per read and **resets after each successful read**, so it catches a
/// never-arriving header or a genuinely stalled stream without capping healthy
/// progress. The tunable, typed stream-timeout system (TTFT / inter-chunk idle
/// / overall wall-clock cap → `StreamError::IdleTimeout`) lives in
/// `octos-agent`'s `streaming.rs` on top of this.
pub fn build_streaming_http_client(connect_timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .read_timeout(std::time::Duration::from_secs(
            DEFAULT_LLM_STREAM_IDLE_TIMEOUT_SECS,
        ))
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("failed to build streaming HTTP client")
}

/// Shared test doubles for wrapper-forwarding tests. `TwoLaneStub` mimics a
/// composite whose slot 1 is a different lane (what `ProviderChain` /
/// `AdaptiveRouter` report after failover); `StubLane` is a concrete lane that
/// either always succeeds or always fails with an HTTP 500.
/// Flat-lane bookkeeping shared by the composites (`ProviderChain`,
/// `FallbackProvider`, `AdaptiveRouter`): the first flat lane index owned by
/// slot `slot`, given every slot's [`LlmProvider::provider_lane_count`].
pub(crate) fn lane_offset_for_slot(lane_counts: &[usize], slot: usize) -> usize {
    lane_counts.iter().take(slot).sum()
}

/// Inverse of [`lane_offset_for_slot`]: the `(slot, inner_index)` a flat
/// lane index belongs to, or `None` when it is out of range.
pub(crate) fn slot_for_lane_index(lane_counts: &[usize], index: usize) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    for (slot, count) in lane_counts.iter().enumerate() {
        if index < offset + count {
            return Some((slot, index - offset));
        }
        offset += count;
    }
    None
}

/// Prefix `stream` with the flat lane index of the serving slot and
/// translate any `ProviderIndex` event a nested composite emits later into
/// the same flat space (`offset + inner`), so the consumer's "last index
/// wins" rule resolves the leaf lane through any composition depth.
pub(crate) fn stream_with_lane_offset(
    offset: usize,
    stream: crate::types::ChatStream,
) -> crate::types::ChatStream {
    use futures::StreamExt;
    Box::pin(
        futures::stream::once(async move { crate::types::StreamEvent::ProviderIndex(offset) })
            .chain(stream.map(move |event| match event {
                crate::types::StreamEvent::ProviderIndex(inner) => {
                    crate::types::StreamEvent::ProviderIndex(offset + inner)
                }
                other => other,
            })),
    )
}

#[cfg(test)]
pub(crate) mod test_lanes {
    use super::*;
    use crate::error::{LlmError, LlmErrorKind};
    use crate::types::{StopReason, TokenUsage};

    /// Both `Display` renderings must name the concrete lane and protocol
    /// and must not misstate the vendor for a compatibility lane.
    pub(crate) fn assert_error_names_lane(
        error: &eyre::Report,
        lane: &str,
        api_style: &str,
        forbidden: &[&str],
    ) {
        for rendered in [error.to_string(), format!("{error:#}")] {
            assert!(rendered.contains(lane), "missing {lane:?} in: {rendered}");
            assert!(
                rendered.contains(api_style),
                "missing {api_style:?} in: {rendered}"
            );
            for phrase in forbidden {
                assert!(
                    !rendered.contains(phrase),
                    "vendor misstatement {phrase:?} in: {rendered}"
                );
            }
        }
    }

    pub(crate) struct TwoLaneStub;

    #[async_trait]
    impl LlmProvider for TwoLaneStub {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            eyre::bail!("TwoLaneStub does not serve requests")
        }

        fn model_id(&self) -> &str {
            "model-a"
        }

        fn provider_name(&self) -> &str {
            "lane-a"
        }

        fn provider_metadata_for_index(&self, provider_index: Option<usize>) -> ProviderMetadata {
            match provider_index {
                Some(1) => ProviderMetadata::new("lane-b", "model-b", Some("b.example".to_owned())),
                _ => self.provider_metadata(),
            }
        }
    }

    pub(crate) struct StubLane {
        provider: &'static str,
        model: &'static str,
        fail: bool,
    }

    impl StubLane {
        pub(crate) fn ok(provider: &'static str, model: &'static str) -> Self {
            Self {
                provider,
                model,
                fail: false,
            }
        }

        pub(crate) fn failing(provider: &'static str, model: &'static str) -> Self {
            Self {
                provider,
                model,
                fail: true,
            }
        }

        fn error(&self) -> eyre::Report {
            LlmError::new(
                LlmErrorKind::ServerError { status: 500 },
                "internal server error",
            )
            .into()
        }
    }

    #[async_trait]
    impl LlmProvider for StubLane {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            if self.fail {
                return Err(self.error());
            }
            Ok(ChatResponse {
                content: Some(format!("from {}", self.provider)),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatStream> {
            if self.fail {
                return Err(self.error());
            }
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta(format!("from {}", self.provider)),
                StreamEvent::Done(StopReason::EndTurn),
            ])))
        }

        fn model_id(&self) -> &str {
            self.model
        }

        fn provider_name(&self) -> &str {
            self.provider
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_error_body_short() {
        let body = "Bad Request: invalid model";
        assert_eq!(truncate_error_body(body), body);
    }

    #[test]
    fn test_truncate_error_body_exact_200() {
        let body = "x".repeat(200);
        assert_eq!(truncate_error_body(&body), body);
    }

    #[test]
    fn test_truncate_error_body_long() {
        let body = "x".repeat(500);
        let result = truncate_error_body(&body);
        assert!(result.starts_with(&"x".repeat(200)));
        assert!(result.contains("500 bytes total"));
        assert!(result.len() < 500);
    }

    #[test]
    fn test_truncate_error_body_empty() {
        assert_eq!(truncate_error_body(""), "");
    }

    #[test]
    fn should_not_panic_when_truncating_multibyte_utf8_body() {
        // Regression: byte-index slicing at 200 could land mid-codepoint and
        // panic on non-ASCII error bodies (e.g. Chinese providers).
        let body = "错误".repeat(500); // each char is 3 bytes, > 200 chars total
        let result = truncate_error_body(&body);
        assert!(result.contains("bytes total"));
        // The kept prefix must itself be valid UTF-8 (no panic, no mojibake).
        assert!(result.starts_with("错误"));
    }

    #[test]
    fn test_build_http_client_succeeds() {
        let _client = build_http_client(30, 10);
        // Just verify it doesn't panic
    }

    #[test]
    fn test_build_streaming_http_client_succeeds() {
        let _client = build_streaming_http_client(10);
        // Just verify it doesn't panic
    }

    /// The core of the streaming-timeout fix: the regular client's **total**
    /// timeout kills a response that takes longer than the deadline, while the
    /// streaming client (no total timeout) does not — a slow-but-healthy
    /// response completes. Regression guard for a slow local model being cut
    /// off mid-generation.
    #[tokio::test]
    async fn streaming_client_is_not_capped_by_total_timeout() {
        use std::time::Duration;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Respond after 2s — longer than the regular client's 1s total timeout,
        // but far under the streaming client's per-read idle timeout.
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("ok")
                    .set_delay(Duration::from_secs(2)),
            )
            .mount(&server)
            .await;

        // Regular client, 1s total timeout: the 2s response exceeds it → error.
        let capped = build_http_client(1, 10);
        let capped_result = capped.get(server.uri()).send().await;
        assert!(
            capped_result.is_err(),
            "regular client with a 1s total timeout must time out on a 2s response"
        );
        assert!(
            capped_result.unwrap_err().is_timeout(),
            "the regular client's failure must be a timeout"
        );

        // Streaming client, no total timeout: the same 2s response completes.
        let streaming = build_streaming_http_client(10);
        let streaming_result = streaming.get(server.uri()).send().await;
        assert!(
            streaming_result.is_ok(),
            "streaming client must not cap a healthy 2s response: {:?}",
            streaming_result.err()
        );
        assert_eq!(streaming_result.unwrap().status(), 200);
    }
}

#[cfg(test)]
mod lane_attribution_helper_tests {
    use std::sync::Arc;

    use super::*;
    use crate::error::{LlmError, LlmErrorKind};

    #[test]
    fn should_render_lane_label_with_api_style_when_known() {
        assert_eq!(
            lane_label("zai-coding", "glm-5.3", Some(ApiStyle::AnthropicMessages)),
            "zai-coding/glm-5.3 (api_style=anthropic_messages)"
        );
        assert_eq!(lane_label("custom", "m", None), "custom/m");
    }

    #[test]
    fn should_name_concrete_lane_in_transport_error_message() {
        assert_eq!(
            transport_error_message(true, "zai-coding", "glm-5.3", ApiStyle::AnthropicMessages),
            "failed to send streaming request to zai-coding/glm-5.3 (api_style=anthropic_messages)"
        );
        assert_eq!(
            transport_error_message(
                false,
                "moonshot-coding@api",
                "k3",
                ApiStyle::OpenAiChatCompletions
            ),
            "failed to send request to moonshot-coding@api/k3 (api_style=openai_chat_completions)"
        );
    }

    #[test]
    fn should_report_api_style_for_every_adapter_and_forward_through_wrappers() {
        assert_eq!(
            crate::anthropic::AnthropicProvider::new("k", "m").api_style(),
            Some(ApiStyle::AnthropicMessages)
        );
        assert_eq!(
            crate::openai::OpenAIProvider::new("k", "m").api_style(),
            Some(ApiStyle::OpenAiChatCompletions)
        );
        assert_eq!(
            crate::openai_responses::OpenAIResponsesProvider::new("k", "m").api_style(),
            Some(ApiStyle::OpenAiResponses)
        );
        assert_eq!(
            crate::gemini::GeminiProvider::new("k", "m").api_style(),
            Some(ApiStyle::GeminiGenerateContent)
        );
        assert_eq!(
            crate::openrouter::OpenRouterProvider::new("k", "m").api_style(),
            Some(ApiStyle::OpenRouterChatCompletions)
        );
        let inner: Arc<dyn LlmProvider> =
            Arc::new(crate::anthropic::AnthropicProvider::new("k", "m"));
        assert_eq!(
            crate::retry::RetryProvider::new(inner.clone()).api_style(),
            Some(ApiStyle::AnthropicMessages)
        );
        assert_eq!(
            crate::middleware::MiddlewareStack::new(inner.clone()).api_style(),
            Some(ApiStyle::AnthropicMessages)
        );
        assert_eq!(
            crate::context_override::ContextWindowOverride::new(inner.clone(), 1).api_style(),
            Some(ApiStyle::AnthropicMessages)
        );
        assert_eq!(
            crate::swappable::SwappableProvider::new(inner).api_style(),
            Some(ApiStyle::AnthropicMessages)
        );
        assert_eq!(test_lanes::TwoLaneStub.api_style(), None);
    }

    #[test]
    fn should_render_operational_messages_uniformly_for_every_stage() {
        let lane = "zai-coding/glm-5.3 (api_style=anthropic_messages)";
        let render = |stage| {
            operational_error_message(stage, "zai-coding", "glm-5.3", ApiStyle::AnthropicMessages)
        };
        assert_eq!(
            render(OperationalStage::SerializeRequest),
            format!("failed to serialize request for {lane}")
        );
        assert_eq!(
            render(OperationalStage::BuildRequestBody),
            format!("failed to build request body for {lane}")
        );
        assert_eq!(
            render(OperationalStage::ReadResponseBody),
            format!("failed to read response body from {lane}")
        );
        assert_eq!(
            render(OperationalStage::ParseResponse),
            format!("failed to parse response from {lane}")
        );
        assert_eq!(
            render(OperationalStage::NoChoices),
            format!("no choices in response from {lane}")
        );
        assert_eq!(
            render(OperationalStage::NoCandidates),
            format!("no candidates in response from {lane}")
        );
        // The vendor word appears exactly when the label is the vendor default.
        assert_eq!(
            operational_error_message(
                OperationalStage::NoChoices,
                "openai",
                "gpt-4o",
                ApiStyle::OpenAiChatCompletions
            ),
            "no choices in response from openai/gpt-4o (api_style=openai_chat_completions)"
        );
    }

    #[test]
    fn should_summarize_every_lane_failure_in_top_level_display_and_keep_typed_chain() {
        let carrier: eyre::Report =
            LlmError::new(LlmErrorKind::ServerError { status: 503 }, "unavailable")
                .with_provider("zai-coding/glm-5.3")
                .into();
        let failures = [
            LaneFailure {
                lane: lane_label(
                    "moonshot-coding@api",
                    "k3",
                    Some(ApiStyle::OpenAiChatCompletions),
                ),
                error: "boom".to_owned(),
            },
            LaneFailure::capture(
                &crate::anthropic::AnthropicProvider::new("k", "glm-5.3")
                    .with_provider_label("zai-coding"),
                &carrier,
            ),
        ];
        let err = attribute_lane_failures(carrier, LANES_EXHAUSTED, &failures);
        let display = err.to_string();
        assert!(
            display.starts_with(
                "all lanes failed: moonshot-coding@api/k3 (api_style=openai_chat_completions): boom; \
                 zai-coding/glm-5.3 (api_style=anthropic_messages): API error (zai-coding/glm-5.3): \
                 provider server error — unavailable"
            ),
            "{display}"
        );
        assert!(
            err.chain()
                .any(|cause| cause.downcast_ref::<LlmError>().is_some()),
            "the typed carrier must stay in the chain"
        );
    }
}
