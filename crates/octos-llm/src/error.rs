//! Typed error hierarchy for LLM operations.
//!
//! Replaces ad-hoc `eyre::Report` usage with structured errors that callers
//! can match on programmatically rather than string-matching error messages.

use std::fmt;

use tracing;

/// Categorized LLM error kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmErrorKind {
    /// Authentication failure (invalid/expired API key).
    Authentication,
    /// Rate limited by the provider (429).
    RateLimited {
        /// Retry-After hint in seconds, if provided.
        retry_after_secs: Option<u64>,
    },
    /// Request exceeded provider's context window or token limit.
    ContextOverflow {
        limit: Option<u32>,
        used: Option<u32>,
    },
    /// Model not found or not accessible.
    ModelNotFound { model: String },
    /// Provider returned a server error (5xx).
    ServerError { status: u16 },
    /// Network/connection error.
    Network,
    /// Request timed out.
    Timeout,
    /// Invalid request (bad parameters, schema, etc.).
    InvalidRequest { detail: String },
    /// Content was filtered by safety/moderation.
    ContentFiltered,
    /// Streaming error (connection drop, malformed SSE, etc.).
    StreamError,
    /// Provider-specific error not covered above.
    Provider { code: Option<String> },
}

/// A structured LLM error with kind, message, and optional source.
#[derive(Debug)]
pub struct LlmError {
    pub kind: LlmErrorKind,
    pub message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl LlmError {
    pub fn new(kind: LlmErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// Convenience: create an Authentication error.
    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(LlmErrorKind::Authentication, message)
    }

    /// Convenience: create a RateLimited error.
    pub fn rate_limited(retry_after_secs: Option<u64>) -> Self {
        Self::new(
            LlmErrorKind::RateLimited { retry_after_secs },
            "rate limited by provider",
        )
    }

    /// Convenience: create a Timeout error.
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(LlmErrorKind::Timeout, message)
    }

    /// Convenience: create a Network error.
    pub fn network(message: impl Into<String>) -> Self {
        Self::new(LlmErrorKind::Network, message)
    }

    /// Returns true if this error is retryable (rate limit, server error, network, timeout).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            LlmErrorKind::RateLimited { .. }
                | LlmErrorKind::ServerError { .. }
                | LlmErrorKind::Network
                | LlmErrorKind::Timeout
                | LlmErrorKind::StreamError
        )
    }

    /// Classify an HTTP status code into an error kind.
    pub fn from_status(status: u16, body: &str) -> Self {
        let kind = match status {
            401 | 403 => LlmErrorKind::Authentication,
            429 => LlmErrorKind::RateLimited {
                retry_after_secs: None,
            },
            404 => LlmErrorKind::ModelNotFound {
                model: String::new(),
            },
            400 => {
                if body.contains("context_length") || body.contains("max_tokens") {
                    LlmErrorKind::ContextOverflow {
                        limit: None,
                        used: None,
                    }
                } else {
                    LlmErrorKind::InvalidRequest {
                        detail: body.chars().take(200).collect(),
                    }
                }
            }
            s if (500..600).contains(&s) => LlmErrorKind::ServerError { status: s },
            _ => LlmErrorKind::Provider { code: None },
        };
        let truncated_body: String = body.chars().take(200).collect();
        tracing::debug!(status, body = %truncated_body, "LLM provider error response");
        Self::new(kind, format!("HTTP {status}"))
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for LlmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|s| s.as_ref() as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_classify_401_as_auth() {
        let err = LlmError::from_status(401, "Unauthorized");
        assert_eq!(err.kind, LlmErrorKind::Authentication);
        assert!(!err.is_retryable());
    }

    #[test]
    fn should_classify_429_as_rate_limited() {
        let err = LlmError::from_status(429, "Too Many Requests");
        assert!(matches!(err.kind, LlmErrorKind::RateLimited { .. }));
        assert!(err.is_retryable());
    }

    #[test]
    fn should_classify_500_as_server_error() {
        let err = LlmError::from_status(500, "Internal Server Error");
        assert!(matches!(
            err.kind,
            LlmErrorKind::ServerError { status: 500 }
        ));
        assert!(err.is_retryable());
    }

    #[test]
    fn should_classify_400_context_overflow() {
        let err = LlmError::from_status(400, "context_length_exceeded");
        assert!(matches!(err.kind, LlmErrorKind::ContextOverflow { .. }));
    }

    #[test]
    fn should_classify_400_invalid_request() {
        let err = LlmError::from_status(400, "invalid parameter: temperature");
        assert!(matches!(err.kind, LlmErrorKind::InvalidRequest { .. }));
    }

    #[test]
    fn should_create_convenience_errors() {
        assert!(!LlmError::auth("bad key").is_retryable());
        assert!(LlmError::rate_limited(Some(30)).is_retryable());
        assert!(LlmError::timeout("timed out").is_retryable());
        assert!(LlmError::network("connection reset").is_retryable());
    }

    #[test]
    fn should_display_error() {
        let err = LlmError::auth("invalid API key");
        let s = err.to_string();
        assert!(s.contains("Authentication"));
        assert!(s.contains("invalid API key"));
    }

    #[test]
    fn should_handle_non_ascii_body_without_panic() {
        // Multi-byte UTF-8 characters that would panic with byte-level slicing
        let body = "あ".repeat(100); // 300 bytes, 100 chars
        let err = LlmError::from_status(500, &body);
        assert!(matches!(
            err.kind,
            LlmErrorKind::ServerError { status: 500 }
        ));
        // Should not panic — truncates at char boundary
        let _ = err.to_string();
    }

    #[test]
    fn should_support_source_chain() {
        use std::error::Error;
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout");
        let err = LlmError::timeout("request timed out").with_source(io_err);
        assert!(err.source().is_some());
    }
}
