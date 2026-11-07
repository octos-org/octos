//! Retry wrapper for LLM providers with exponential backoff.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use tracing::{debug, warn};

use crate::config::ChatConfig;
use crate::error::{LlmError, LlmErrorKind};
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, ToolSpec};

/// Configuration for retry behavior.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts.
    pub max_retries: u32,
    /// Initial delay between retries.
    pub initial_delay: Duration,
    /// Maximum delay between retries.
    pub max_delay: Duration,
    /// Multiplier for exponential backoff.
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            backoff_multiplier: 2.0,
        }
    }
}

/// Wrapper that adds retry logic to any LLM provider.
pub struct RetryProvider {
    inner: Arc<dyn LlmProvider>,
    config: RetryConfig,
}

impl RetryProvider {
    /// Create a new retry provider wrapping an existing provider.
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            inner: provider,
            config: RetryConfig::default(),
        }
    }

    /// Set custom retry configuration.
    pub fn with_config(mut self, config: RetryConfig) -> Self {
        self.config = config;
        self
    }

    /// Check if an error should trigger failover to the next provider.
    ///
    /// This is broader than `is_retryable_error`: auth failures (401/403)
    /// should not be retried on the *same* provider but should failover to
    /// a different provider which may have valid credentials.
    ///
    /// Codex round-2 BLOCKER fix: prior versions only string-matched the
    /// legacy `"API error: <code>"` shape. The typed `LlmError::Display`
    /// renders as `"API error (<provider>): <summary> — HTTP <code> ..."`
    /// which the legacy match misses, so `FallbackProvider` and
    /// `ProviderChain` failed to failover for Quota / Auth / Rate-Limited /
    /// Server-Error classifications. We now downcast to `LlmError` first
    /// and switch on `LlmErrorKind` directly; the legacy string-match
    /// branch is preserved for non-typed callers and bare `eyre::eyre!`
    /// reports.
    pub(crate) fn should_failover(error: &eyre::Report) -> bool {
        // Typed-error path: walk the cause chain and switch on the
        // structured kind. This is the canonical entry point — once a
        // provider emits `LlmError::from_status_with_label`, this branch
        // wins.
        for cause in error.chain() {
            if let Some(llm) = cause.downcast_ref::<LlmError>() {
                return match &llm.kind {
                    // Quota is a per-lane/provider credential failure
                    // (like Auth): the *same* provider won't recover on
                    // retry (won't be `is_retryable`), but another
                    // configured lane may have a different key/account
                    // with available quota. Codex round-3 BLOCKER fix:
                    // previous version returned `false` here which
                    // collapsed the entire chain when the primary lane
                    // ran out of billing.
                    LlmErrorKind::Quota => true,
                    // The current provider's credentials are bad — try
                    // the next lane which may have a valid key.
                    LlmErrorKind::Authentication => true,
                    LlmErrorKind::RateLimited { .. } => true,
                    // BadRequest / InvalidRequest is failover-worthy:
                    // e.g. deepseek's `reasoning_content` 400 may pass
                    // through openai-compat lanes with different
                    // validation rules.
                    LlmErrorKind::InvalidRequest { .. } => true,
                    LlmErrorKind::ServerError { status } => (500..600).contains(status),
                    // Transient — failover gives us a chance to swap to
                    // a healthy lane while the primary recovers.
                    LlmErrorKind::Network
                    | LlmErrorKind::Timeout
                    | LlmErrorKind::StreamError
                    | LlmErrorKind::ContextOverflow { .. } => true,
                    // 4xx other / generic provider error — keep current
                    // provider, the next one will hit the same wall.
                    LlmErrorKind::ContentFiltered
                    | LlmErrorKind::ModelNotFound { .. }
                    | LlmErrorKind::Provider { .. } => false,
                };
            }
        }

        // Auth errors and timeouts: don't retry same provider, but do failover
        for cause in error.chain() {
            if let Some(reqwest_err) = cause.downcast_ref::<reqwest::Error>() {
                // Timeout → failover immediately (don't waste 120s × retries)
                if reqwest_err.is_timeout() {
                    return true;
                }
                if let Some(status) = reqwest_err.status() {
                    if matches!(status.as_u16(), 401 | 403) {
                        return true;
                    }
                }
            }
        }
        let error_str = error.to_string();
        for code in ["401", "403"] {
            if error_str.contains(&format!("API error: {code}")) {
                return true;
            }
        }

        // Content-format and auth 400 errors: the request may work with a
        // different provider that has different validation rules.
        if error_str.contains("400")
            && (error_str.contains("must not be empty")
                || error_str.contains("reasoning_content")
                || error_str.contains("API key not valid")
                || error_str.contains("invalid_value"))
        {
            return true;
        }

        // Flattened third-party timeouts have no reqwest/LlmError type left.
        // Match reqwest timeout semantics: never retry the same lane, but allow
        // the chain/router to try a different lane. Typed LlmError policy is unchanged.
        if error_str.to_lowercase().contains("timed out") {
            return true;
        }

        // Everything retryable is also failover-worthy
        Self::is_retryable_error(error)
    }

    /// Check if an error is retryable on the same provider.
    ///
    /// First tries to extract an HTTP status code from the error chain
    /// (reqwest errors carry status). Falls back to keyword matching for
    /// non-HTTP errors like connection failures.
    pub(crate) fn is_retryable_error(error: &eyre::Report) -> bool {
        // Typed-error path: the new provider error path constructs
        // `LlmError` directly. Honor `is_retryable()` so RateLimited /
        // ServerError / Network / Timeout / StreamError get the same
        // backoff treatment they had under the legacy string match.
        for cause in error.chain() {
            if let Some(llm) = cause.downcast_ref::<LlmError>() {
                return llm.is_retryable();
            }
        }
        // Check for reqwest errors with status codes (most reliable)
        for cause in error.chain() {
            if let Some(reqwest_err) = cause.downcast_ref::<reqwest::Error>() {
                if let Some(status) = reqwest_err.status() {
                    return matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504 | 529);
                }
                // Timeout errors should NOT be retried on the same provider —
                // if a provider is unresponsive, retrying wastes the
                // per-request budget × retries. Timeouts trigger failover to a
                // different provider instead. Checked BEFORE `is_connect`: a
                // connect *timeout* is BOTH `is_timeout()` and `is_connect()`,
                // and must fail over rather than hammer the same unreachable
                // lane (empirically confirmed against a black-holed address).
                if reqwest_err.is_timeout() {
                    return false;
                }
                // Connection *establishment* errors with no timeout (refused,
                // DNS, TLS handshake) are transient — retry on the same
                // provider (may be a transient network blip).
                if reqwest_err.is_connect() {
                    return true;
                }
                // Transport-level send/body failures WITHOUT an HTTP status —
                // the status-bearing branch above already returned, so any
                // reqwest error reaching here carried no response. These are
                // transient network faults while sending the request or
                // reading the response head: connection reset, broken pipe,
                // early EOF, and most importantly "connection closed before
                // message completed" — reqwest reusing a pooled keepalive
                // socket that the peer's load balancer already closed. reqwest
                // classifies these as `is_request()`/`is_body()` (NOT
                // `is_connect`, NOT `is_timeout`), so the connect-only branch
                // above missed them and a mid-turn drop hard-failed the whole
                // turn with no retry and no failover. Retry on the same
                // provider — a fresh connection is dialed on the next attempt.
                //
                // DELIBERATE at-least-once tradeoff: a statusless send failure
                // is ambiguous — the request may have been fully received and
                // billed by the server before the connection dropped ("no
                // response" != "not accepted"). Replaying can therefore
                // double-bill a non-idempotent chat POST in the rare
                // mid-generation-drop sub-case. We accept this because (a) the
                // dominant cause here is a reused idle-keepalive socket the LB
                // closed BEFORE servicing the request (never billed → safe to
                // replay), and for a streaming POST a server that had begun
                // generating would have already flushed response headers, so
                // `.send()` would have resolved and the drop would surface as a
                // stream/body error OUTSIDE this retry scope; (b) the agent
                // loop consumes exactly one ChatResponse, so a replay never
                // duplicates tool side-effects — the only residual harm is
                // provider-side double-billing; (c) the alternative is hard-
                // failing the entire turn on any transient drop, which is
                // strictly worse UX. Future hardening (not done here): a short
                // pool idle-timeout to stop reusing about-to-close sockets, or
                // a client idempotency key where the endpoint supports one.
                if reqwest_err.is_request() || reqwest_err.is_body() {
                    return true;
                }
            }
        }

        // Fallback: match on formatted error for provider bail! messages
        // e.g. "Anthropic API error: 429 - ..."
        let error_str = error.to_string();
        for code in ["429", "500", "502", "503", "504", "529"] {
            if error_str.contains(&format!("API error: {code}")) {
                return true;
            }
        }

        // Network-level errors without reqwest context (flattened error
        // strings, provider `bail!`s, or a cause chain that lost the typed
        // reqwest error). "connection closed before message completed" is the
        // reused-keepalive drop; "error sending request" / "broken pipe" cover
        // the same transport family surfaced as plain text.
        let lower = error_str.to_lowercase();
        // Timeout takes negative precedence over generic flat transport text
        // such as "error sending request": an unresponsive lane must not be
        // retried merely because another retryable substring is also present.
        if lower.contains("timed out") {
            return false;
        }
        if lower.contains("connection refused")
            || lower.contains("connection reset")
            || lower.contains("connection closed")
            || lower.contains("broken pipe")
            || lower.contains("error sending request")
            || lower.contains("overloaded")
        {
            return true;
        }

        false
    }

    fn calculate_delay(&self, attempt: u32) -> Duration {
        let delay = self.config.initial_delay.as_secs_f64()
            * self.config.backoff_multiplier.powi(attempt as i32);
        let delay = Duration::from_secs_f64(delay);
        std::cmp::min(delay, self.config.max_delay)
    }

    /// Extract a longer delay for rate-limit (429 TPM) errors.
    /// OpenAI errors include "Please try again in 29.159s" — parse that.
    /// Falls back to 30s if unparseable. Always clamped to `max_delay`.
    fn rate_limit_delay(&self, error: &eyre::Report) -> Option<Duration> {
        let msg = error.to_string();
        // Only apply to rate-limit / TPM errors
        let msg_lower = msg.to_lowercase();
        if !msg_lower.contains("429")
            && !msg_lower.contains("rate limit")
            && !msg_lower.contains("tokens per min")
            && !msg_lower.contains("too many requests")
            && !msg_lower.contains("resource_exhausted")
        {
            return None;
        }
        // Try to parse "try again in Xs" / "X.XXXs" / "Xms". The numeric run
        // stops at the first unit letter; the UNIT that follows decides the
        // scale. Consuming only the digits and assuming seconds turned a
        // sub-second "try again in 906ms" hint into a ~15-minute sleep.
        if let Some(idx) = msg.find("try again in ") {
            let after = &msg[idx + "try again in ".len()..];
            let num_str: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(value) = num_str.parse::<f64>() {
                // `num_str` is ASCII digits/'.', so its char len == byte len.
                let unit = after[num_str.len()..].trim_start();
                let seconds = if unit.starts_with("ms") {
                    value / 1000.0
                } else {
                    // "s", "sec", or a bare number → seconds (OpenAI/Anthropic
                    // spell sub-second hints in ms, so a unit-less value is a
                    // whole-second count).
                    value
                };
                // The hint is untrusted and can be non-finite or exceed
                // Duration's range. Use the checked conversion, then clamp as a
                // Duration (converting Duration::MAX to f64 would round UP and
                // reintroduce the panic).
                let base = Duration::try_from_secs_f64(seconds)
                    .map(|delay| delay.min(self.config.max_delay))
                    .unwrap_or(self.config.max_delay);
                // Add a 1s buffer, then clamp to `max_delay` so a large or
                // malformed hint ("try again in 1800s") can't park the whole
                // provider on a multi-minute sleep past the configured ceiling.
                let delay = base.saturating_add(Duration::from_secs(1));
                return Some(delay.min(self.config.max_delay));
            }
        }
        // Fallback: wait 30s for TPM to reset (still clamped to max_delay).
        Some(Duration::from_secs(30).min(self.config.max_delay))
    }
}

#[async_trait]
impl LlmProvider for RetryProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        for attempt in 0..=self.config.max_retries {
            match self.inner.chat(messages, tools, config).await {
                Ok(response) => {
                    if attempt > 0 {
                        debug!(attempt, "request succeeded after retry");
                    }
                    return Ok(response);
                }
                Err(e) => {
                    let fail_fast =
                        crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast;
                    if !fail_fast
                        && attempt < self.config.max_retries
                        && Self::is_retryable_error(&e)
                    {
                        let delay = self
                            .rate_limit_delay(&e)
                            .unwrap_or_else(|| self.calculate_delay(attempt));
                        warn!(
                            attempt = attempt + 1,
                            max_retries = self.config.max_retries,
                            delay_secs = delay.as_secs_f64(),
                            error = %e,
                            "retryable error, backing off"
                        );
                        tokio::time::sleep(delay).await;
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        // Structurally unreachable: every iteration returns Ok or the final
        // attempt returns Err directly. Kept as a defensive fallback.
        eyre::bail!("retry loop exited unexpectedly")
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        for attempt in 0..=self.config.max_retries {
            match self.inner.chat_stream(messages, tools, config).await {
                Ok(stream) => {
                    if attempt > 0 {
                        debug!(attempt, "stream request succeeded after retry");
                    }
                    return Ok(stream);
                }
                Err(e) => {
                    let fail_fast =
                        crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast;
                    if !fail_fast
                        && attempt < self.config.max_retries
                        && Self::is_retryable_error(&e)
                    {
                        let delay = self
                            .rate_limit_delay(&e)
                            .unwrap_or_else(|| self.calculate_delay(attempt));
                        warn!(
                            attempt = attempt + 1,
                            max_retries = self.config.max_retries,
                            delay_secs = delay.as_secs_f64(),
                            error = %e,
                            "retryable stream error, backing off"
                        );
                        tokio::time::sleep(delay).await;
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        // Structurally unreachable: see chat() above.
        eyre::bail!("retry loop exited unexpectedly")
    }

    // #2135 review P1: without these delegations the trait defaults re-read
    // the static catalog by model id, silently discarding a probed or
    // overridden window on the standard runtime path (every session wraps
    // the base provider in RetryProvider).
    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }

    fn estimate_request_tokens(
        &self,
        messages: &[Message],
        tools: &[crate::types::ToolSpec],
    ) -> u32 {
        // #2143 part 3: delegate so a concrete provider's request-size override
        // survives this wrapper (mirrors context_window delegation).
        self.inner.estimate_request_tokens(messages, tools)
    }

    fn max_output_tokens(&self) -> u32 {
        self.inner.max_output_tokens()
    }

    async fn ensure_ready(&self) {
        self.inner.ensure_ready().await;
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn provider_metadata(&self) -> crate::ProviderMetadata {
        // #2194 R4: transparent wrapper — carry the inner slot's cache lane
        // (and identity) through, or pricing sees the default Residual lane.
        self.inner.provider_metadata()
    }

    fn provider_metadata_for_index(
        &self,
        provider_index: Option<usize>,
    ) -> crate::types::ProviderMetadata {
        self.inner.provider_metadata_for_index(provider_index)
    }

    fn provider_lane_count(&self) -> usize {
        self.inner.provider_lane_count()
    }

    fn api_style(&self) -> Option<crate::provider::ApiStyle> {
        self.inner.api_style()
    }

    fn supports_semantic_checkpoint_hints(&self) -> bool {
        self.inner.supports_semantic_checkpoint_hints()
    }

    fn report_late_failure(&self) {
        self.inner.report_late_failure();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ──────────────────────────────────────────────────────────────────────
    // Codex round-2 BLOCKER regression: pin each `LlmErrorKind` branch so
    // the typed-error path stays in sync with `should_failover`. These
    // tests would have caught the original bug where the typed Display
    // string (`"API error (<provider>): … — HTTP 403 …"`) didn't match
    // the legacy `"API error: 403"` string-match, so 403/quota/auth/etc.
    // silently bypassed failover.
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn test_not_retryable_typed_quota() {
        let llm = LlmError::new(LlmErrorKind::Quota, "out of credits");
        let err: eyre::Report = llm.into();
        assert!(!RetryProvider::is_retryable_error(&err));
    }

    #[test]
    fn test_rate_limit_delay_clamps_to_max_delay() {
        // A large (or malformed) hint cannot exceed the configured ceiling.
        let provider = RetryProvider {
            inner: Arc::new(MockProvider),
            config: RetryConfig {
                max_delay: Duration::from_secs(60),
                ..RetryConfig::default()
            },
        };
        let err =
            eyre::eyre!("OpenAI API error: 429 Too Many Requests - Please try again in 1800s");
        let delay = provider.rate_limit_delay(&err).unwrap();
        assert_eq!(delay, Duration::from_secs(60), "must clamp to max_delay");
    }

    // ──────────────────────────────────────────────────────────────────────
    // FailFast policy tests
    // ──────────────────────────────────────────────────────────────────────

    /// Provider that always returns a retryable error and counts `chat` and
    /// `chat_stream` calls via a shared counter.
    struct CountingProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingProvider {
        fn always_err_429() -> Self {
            Self {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for CountingProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(LlmError::rate_limited(Some(0)).into())
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatStream> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(LlmError::rate_limited(Some(0)).into())
        }

        fn model_id(&self) -> &str {
            "counting"
        }

        fn provider_name(&self) -> &str {
            "test"
        }
    }

    #[tokio::test]
    async fn should_call_inner_once_when_failfast_on_stream() {
        use crate::{LlmCallPolicy, with_llm_call_policy};
        use std::sync::atomic::Ordering;
        let provider = CountingProvider::always_err_429();
        let calls = provider.calls.clone();
        let retry = RetryProvider::new(Arc::new(provider)).with_config(RetryConfig {
            max_retries: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            backoff_multiplier: 2.0,
        });

        let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
            retry.chat_stream(&[], &[], &ChatConfig::default()).await
        })
        .await;

        assert!(result.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "FailFast must not retry chat_stream"
        );
    }

    struct MockProvider;

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            unimplemented!()
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    /// Provider that fails N times with a retryable error, then succeeds.
    struct FailingStreamProvider {
        remaining_failures: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl LlmProvider for FailingStreamProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            unimplemented!()
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatStream> {
            let remaining = self
                .remaining_failures
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            if remaining > 0 {
                eyre::bail!("API error: 503 - service unavailable");
            }
            // Return an empty stream on success
            let stream = futures::stream::empty();
            Ok(Box::pin(stream))
        }

        fn model_id(&self) -> &str {
            "failing-stream"
        }

        fn provider_name(&self) -> &str {
            "test"
        }
    }

    #[tokio::test]
    async fn test_chat_stream_retries_on_503() {
        let provider = RetryProvider {
            inner: Arc::new(FailingStreamProvider {
                remaining_failures: std::sync::atomic::AtomicU32::new(2), // fail twice, then succeed
            }),
            config: RetryConfig {
                max_retries: 3,
                initial_delay: Duration::from_millis(1), // fast for tests
                max_delay: Duration::from_millis(10),
                backoff_multiplier: 1.0,
            },
        };

        let result = provider.chat_stream(&[], &[], &ChatConfig::default()).await;
        assert!(result.is_ok(), "should succeed after retries");
    }

    #[tokio::test]
    async fn test_chat_stream_exhausts_retries() {
        let provider = RetryProvider {
            inner: Arc::new(FailingStreamProvider {
                remaining_failures: std::sync::atomic::AtomicU32::new(10), // always fail
            }),
            config: RetryConfig {
                max_retries: 2,
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_multiplier: 1.0,
            },
        };

        let result = provider.chat_stream(&[], &[], &ChatConfig::default()).await;
        match result {
            Err(e) => assert!(e.to_string().contains("503"), "unexpected error: {e}"),
            Ok(_) => panic!("should fail after exhausting retries"),
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // Transport-level send-failure classification (issue: glm-5.2
    // `failed to send streaming request` hard-failed a 20-action turn).
    //
    // These drive a real `reqwest` client at a local TCP server that fails
    // the request in the same way z.ai's endpoint does under load, then wrap
    // the error EXACTLY like `anthropic.rs`
    // (`.send().await.wrap_err(crate::provider::transport_error_message(true, "zai", "glm-5.2", crate::provider::ApiStyle::AnthropicMessages))`)
    // and assert the retry/failover verdict.
    // ──────────────────────────────────────────────────────────────────────
    use eyre::WrapErr;

    /// Produce a real `reqwest` send failure of the requested `kind`, wrapped
    /// like the Anthropic provider does:
    ///   - `"refused"`         → nothing listening (reqwest `is_connect`)
    ///   - `"immediate_close"` → server accepts then drops the socket
    ///   - `"read_then_close"` → server reads the request then closes without
    ///     replying ("connection closed before message completed" — the
    ///     reused-keepalive / half-open-socket case)
    async fn transport_send_error(kind: &str) -> eyre::Report {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;

        // Budgets sit far above the platform's loopback-refusal latency:
        // Windows typically answers a connect to a just-closed port only
        // after ~2s (occasionally slower), and the assertions below
        // require the error to not be a timeout.
        let client = crate::provider::build_http_client(30, 30);

        if kind == "refused" {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let url = format!("http://127.0.0.1:{port}/v1/messages");
            let res = client
                .post(&url)
                .json(&serde_json::json!({"x": 1}))
                .send()
                .await;
            return res
                .map(|_| ())
                .wrap_err(crate::provider::transport_error_message(
                    true,
                    "zai",
                    "glm-5.2",
                    crate::provider::ApiStyle::AnthropicMessages,
                ))
                .unwrap_err();
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let read_then_close = kind == "read_then_close";
        let accept = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                if read_then_close {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                }
                drop(sock);
            }
        });

        let url = format!("http://127.0.0.1:{port}/v1/messages");
        let res = client
            .post(&url)
            .json(&serde_json::json!({"x": 1}))
            .send()
            .await;
        let _ = accept.await;
        res.map(|_| ())
            .wrap_err(crate::provider::transport_error_message(
                true,
                "zai",
                "glm-5.2",
                crate::provider::ApiStyle::AnthropicMessages,
            ))
            .unwrap_err()
    }

    #[tokio::test]
    async fn should_retry_and_failover_on_connection_closed_before_completed() {
        // The exact failure z.ai returns when reqwest reuses a keepalive
        // socket the load balancer already closed: reqwest reports it as
        // `is_request()` (NOT `is_connect`, NOT `is_timeout`, no status).
        // Before the fix this was classified non-retryable AND
        // non-failover-worthy, hard-failing the whole turn.
        let err = transport_send_error("read_then_close").await;
        assert!(
            RetryProvider::is_retryable_error(&err),
            "statusless transport send failure must retry on the same provider: {err:#}"
        );
        assert!(
            RetryProvider::should_failover(&err),
            "statusless transport send failure must also be failover-worthy: {err:#}"
        );
    }
}

#[cfg(test)]
mod provider_metadata_tests {}

#[cfg(test)]
mod lane_summary_classification_tests {}
