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
    fn provider_metadata_for_index(&self, _provider_index: Option<usize>) -> ProviderMetadata {
        self.provider_metadata()
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
