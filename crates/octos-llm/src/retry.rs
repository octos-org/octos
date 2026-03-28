//! Retry wrapper for LLM providers with exponential backoff.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use tracing::{debug, warn};

use crate::config::ChatConfig;
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
    pub(crate) fn should_failover(error: &eyre::Report) -> bool {
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

        // Everything retryable is also failover-worthy
        Self::is_retryable_error(error)
    }

    /// Check if an error is retryable on the same provider.
    ///
    /// First tries to extract an HTTP status code from the error chain
    /// (reqwest errors carry status). Falls back to keyword matching for
    /// non-HTTP errors like connection failures.
    pub(crate) fn is_retryable_error(error: &eyre::Report) -> bool {
        // Check for reqwest errors with status codes (most reliable)
        for cause in error.chain() {
            if let Some(reqwest_err) = cause.downcast_ref::<reqwest::Error>() {
                if let Some(status) = reqwest_err.status() {
                    return matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504 | 529);
                }
                // Connection errors are retryable (may be transient network issue)
                if reqwest_err.is_connect() {
                    return true;
                }
                // Timeout errors should NOT be retried on the same provider —
                // if a provider is unresponsive, retrying wastes 120s × retries.
                // Timeouts trigger failover to a different provider instead.
                if reqwest_err.is_timeout() {
                    return false;
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

        // Network-level errors without reqwest context
        let lower = error_str.to_lowercase();
        if lower.contains("connection refused")
            || lower.contains("connection reset")
            || lower.contains("timed out")
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
    /// Falls back to 30s if unparseable.
    fn rate_limit_delay(error: &eyre::Report) -> Option<Duration> {
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
        // Try to parse "try again in Xs" or "try again in X.XXXs"
        if let Some(idx) = msg.find("try again in ") {
            let after = &msg[idx + "try again in ".len()..];
            let num_str: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(secs) = num_str.parse::<f64>() {
                // Add 1s buffer
                return Some(Duration::from_secs_f64(secs + 1.0));
            }
        }
        // Fallback: wait 30s for TPM to reset
        Some(Duration::from_secs(30))
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
                    if attempt < self.config.max_retries && Self::is_retryable_error(&e) {
                        let delay = Self::rate_limit_delay(&e)
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
                    if attempt < self.config.max_retries && Self::is_retryable_error(&e) {
                        let delay = Self::rate_limit_delay(&e)
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

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn report_late_failure(&self) {
        self.inner.report_late_failure();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_retryable_429() {
        let err = eyre::eyre!("Anthropic API error: 429 - rate limited");
        assert!(RetryProvider::is_retryable_error(&err));
    }

    #[test]
    fn test_is_retryable_500() {
        let err = eyre::eyre!("OpenAI API error: 500 - internal server error");
        assert!(RetryProvider::is_retryable_error(&err));
    }

    #[test]
    fn test_is_retryable_503() {
        let err = eyre::eyre!("Gemini API error: 503 - service unavailable");
        assert!(RetryProvider::is_retryable_error(&err));
    }

    #[test]
    fn test_is_retryable_connection() {
        let err = eyre::eyre!("connection refused");
        assert!(RetryProvider::is_retryable_error(&err));
    }

    #[test]
    fn test_is_retryable_overloaded() {
        let err = eyre::eyre!("API overloaded");
        assert!(RetryProvider::is_retryable_error(&err));
    }

    #[test]
    fn test_not_retryable_401() {
        let err = eyre::eyre!("API error: 401 - unauthorized");
        assert!(!RetryProvider::is_retryable_error(&err));
    }

    #[test]
    fn test_not_retryable_400() {
        let err = eyre::eyre!("API error: 400 - bad request");
        assert!(!RetryProvider::is_retryable_error(&err));
    }

    #[test]
    fn test_not_retryable_generic() {
        let err = eyre::eyre!("invalid JSON in response");
        assert!(!RetryProvider::is_retryable_error(&err));
    }

    #[test]
    fn test_should_failover_401() {
        let err = eyre::eyre!("OpenAI API error: 401 - unauthorized");
        assert!(!RetryProvider::is_retryable_error(&err));
        assert!(RetryProvider::should_failover(&err));
    }

    #[test]
    fn test_should_failover_403() {
        let err = eyre::eyre!("API error: 403 - forbidden");
        assert!(!RetryProvider::is_retryable_error(&err));
        assert!(RetryProvider::should_failover(&err));
    }

    #[test]
    fn test_should_failover_429() {
        let err = eyre::eyre!("API error: 429 - rate limited");
        assert!(RetryProvider::should_failover(&err));
    }

    #[test]
    fn test_should_not_failover_400() {
        let err = eyre::eyre!("API error: 400 - bad request");
        assert!(!RetryProvider::should_failover(&err));
    }

    #[test]
    fn test_should_failover_400_content_empty() {
        let err = eyre::eyre!(
            "OpenAI API error: 400 Bad Request - the message with role 'assistant' must not be empty"
        );
        assert!(RetryProvider::should_failover(&err));
    }

    #[test]
    fn test_rate_limit_delay_parses_seconds() {
        let err = eyre::eyre!(
            "OpenAI API error: 429 Too Many Requests - Rate limit reached. Please try again in 29.159s"
        );
        let delay = RetryProvider::rate_limit_delay(&err).unwrap();
        // 29.159 + 1.0 buffer = ~30.159s
        assert!(delay.as_secs_f64() > 29.0 && delay.as_secs_f64() < 32.0);
    }

    #[test]
    fn test_rate_limit_delay_fallback() {
        let err =
            eyre::eyre!("OpenAI API error: 429 Too Many Requests - tokens per min limit exceeded");
        let delay = RetryProvider::rate_limit_delay(&err).unwrap();
        assert_eq!(delay, Duration::from_secs(30));
    }

    #[test]
    fn test_rate_limit_delay_not_429() {
        let err = eyre::eyre!("OpenAI API error: 500 Internal Server Error");
        assert!(RetryProvider::rate_limit_delay(&err).is_none());
    }

    #[test]
    fn test_calculate_delay() {
        let provider = RetryProvider {
            inner: Arc::new(MockProvider),
            config: RetryConfig {
                initial_delay: Duration::from_secs(1),
                max_delay: Duration::from_secs(60),
                backoff_multiplier: 2.0,
                ..Default::default()
            },
        };

        assert_eq!(provider.calculate_delay(0), Duration::from_secs(1));
        assert_eq!(provider.calculate_delay(1), Duration::from_secs(2));
        assert_eq!(provider.calculate_delay(2), Duration::from_secs(4));
        assert_eq!(provider.calculate_delay(3), Duration::from_secs(8));
        // Should cap at max_delay
        assert_eq!(provider.calculate_delay(10), Duration::from_secs(60));
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
                ..Default::default()
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
                ..Default::default()
            },
        };

        let result = provider.chat_stream(&[], &[], &ChatConfig::default()).await;
        match result {
            Err(e) => assert!(e.to_string().contains("503"), "unexpected error: {e}"),
            Ok(_) => panic!("should fail after exhausting retries"),
        }
    }
}
