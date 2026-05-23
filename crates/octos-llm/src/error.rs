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
    /// Provider quota exhausted / billing-tier expired / no active package.
    /// Sourced from HTTP 402 (Payment Required), or 403/429 bodies that
    /// carry an explicit billing marker (`insufficient_quota`,
    /// `quota_exceeded`, `no_active_*_package`) or a billing-class
    /// keyword (`billing`, `monthly`, `spend`, `package`, `credit`).
    /// Distinct from `Authentication` so operators see a "top up or
    /// switch provider" message rather than "bad API key", and distinct
    /// from `RateLimited` so the failover ladder skips the same-provider
    /// backoff and tries the next configured lane (which may have a
    /// different account/key with available billing).
    Quota,
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
///
/// `provider` carries the operator-visible label (e.g. "anthropic",
/// "MiniMax-M2.5-highspeed") so user-facing messages identify which lane
/// hit the failure without leaking secrets. Default is empty when the
/// constructor does not have a label handy.
#[derive(Debug)]
pub struct LlmError {
    pub kind: LlmErrorKind,
    pub message: String,
    /// Operator-visible provider/model label (e.g. "anthropic",
    /// "MiniMax-M2.5-highspeed"). Empty when not provided.
    pub provider: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl LlmError {
    pub fn new(kind: LlmErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            provider: String::new(),
            source: None,
        }
    }

    /// Builder: attach the operator-visible provider/model label.
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
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

    /// Lowercased body markers that flag a quota / billing-tier failure.
    /// Kept private so the classification stays the canonical contract for
    /// `Quota` recognition.
    ///
    /// Codex round-3 MAJOR narrowing: bare `RESOURCE_EXHAUSTED` (Google /
    /// Vertex) is ambiguous — it covers both billing exhaustion *and*
    /// transient capacity. Treating it unconditionally as `Quota` blocks
    /// retry/backoff AND (now that `Quota` triggers failover in
    /// `RetryProvider::should_failover`) burns the next lane on what may
    /// have been a recoverable load shed. We now require a billing-class
    /// keyword (`billing`, `monthly`, `spend`, `package`, `credit`) to
    /// flag `RESOURCE_EXHAUSTED` as quota; bare `RESOURCE_EXHAUSTED`
    /// falls through to `RateLimited` so the same-provider backoff path
    /// can drain transient capacity blips.
    ///
    /// Recognised explicit quota markers (no body-keyword combo needed):
    ///   * `insufficient_quota` — OpenAI billing exhausted
    ///   * `quota_exceeded` — generic
    ///   * `no_active_wisemodel_package` — Wisemodel billing tier expired
    ///   * `no_active_*_package` — generalised Wisemodel-style marker
    ///
    /// Recognised billing-keyword fallbacks (any one is sufficient — these
    /// surface across Anthropic 402, MiniMax 429, etc.):
    ///   * `billing`, `monthly`, `spend`, `package`, `credit`
    fn body_signals_quota(body_lower: &str) -> bool {
        body_lower.contains("insufficient_quota")
            || body_lower.contains("quota_exceeded")
            || body_lower.contains("no_active_wisemodel_package")
            || (body_lower.contains("no_active_") && body_lower.contains("_package"))
            || body_lower.contains("billing")
            || body_lower.contains("monthly")
            || body_lower.contains("spend")
            || body_lower.contains("package")
            || body_lower.contains("credit")
    }

    /// Classify an HTTP status code into an error kind (legacy 2-arg form
    /// kept for tests and downstream callers without a provider label).
    pub fn from_status(status: u16, body: &str) -> Self {
        Self::from_status_with_label(status, body, "")
    }

    /// Classify an HTTP status code into an error kind, capturing the
    /// operator-visible provider/model label for user-facing messages.
    /// Primary entry point for provider HTTP error handlers — replaces the
    /// previous `eyre::bail!("API error ({label}): ...")` pattern so the
    /// loop-boundary classifier can downcast to `LlmError` and pick the
    /// right `HarnessError` variant (issue: variant=internal recovery=bug
    /// for Wisemodel 403 quota errors).
    pub fn from_status_with_label(status: u16, body: &str, provider: impl Into<String>) -> Self {
        let body_lower = body.to_ascii_lowercase();
        let kind = match status {
            401 => LlmErrorKind::Authentication,
            // 402 Payment Required → Quota. Anthropic and other providers
            // use this status code for billing / payment issues
            // (inactive subscription, expired card, etc.). Map to Quota
            // so the operator sees a "top up or switch provider" message
            // rather than "bad key".
            402 => LlmErrorKind::Quota,
            403 => {
                // Disambiguate 403: quota / billing-tier failures get their
                // own variant so the operator sees a "top up or switch
                // provider" message instead of "bad API key".
                if Self::body_signals_quota(&body_lower) {
                    LlmErrorKind::Quota
                } else {
                    LlmErrorKind::Authentication
                }
            }
            429 => {
                // Codex round-2 MAJOR 1: 429 is overloaded across providers.
                // Disambiguate the same way 403 already does:
                //   * OpenAI 429 + `insufficient_quota` → exhausted billing
                //   * Gemini 429 + `RESOURCE_EXHAUSTED` + billing keyword
                //     → exhausted billing
                //   * Gemini 429 + bare `RESOURCE_EXHAUSTED` (no billing
                //     keyword) → real rate limit (capacity / transient)
                //   * Wisemodel 429 + `no_active_*_package` → exhausted billing
                //   * Anthropic 429 + `rate_limit_error` → real rate limit
                //   * Bare 429 (no marker) → real rate limit (legacy default)
                // Without this branch the failover ladder treats `Quota` as
                // a retryable RateLimited and burns the next lane on
                // identical billing failures. Codex round-3 narrowed the
                // `RESOURCE_EXHAUSTED` arm so transient capacity blips
                // stay on the same provider's backoff path.
                if Self::body_signals_quota(&body_lower) {
                    LlmErrorKind::Quota
                } else {
                    LlmErrorKind::RateLimited {
                        retry_after_secs: None,
                    }
                }
            }
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
        let provider = provider.into();
        // Keep the message short so the operator log line stays readable;
        // the full body is available in the wire envelope on the SPA side.
        let message = if truncated_body.is_empty() {
            format!("HTTP {status}")
        } else {
            format!("HTTP {status} - {truncated_body}")
        };
        Self {
            kind,
            message,
            provider,
            source: None,
        }
    }
}

impl fmt::Display for LlmError {
    /// Human-readable rendering. Provider label is prefixed when non-empty
    /// so the operator sees which lane failed (e.g.
    /// "API error (MiniMax-M2.5-highspeed): provider quota exhausted — HTTP 403 - ...").
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = if self.provider.is_empty() {
            "API error".to_string()
        } else {
            format!("API error ({})", self.provider)
        };
        let summary = match &self.kind {
            LlmErrorKind::Authentication => "authentication failed",
            LlmErrorKind::Quota => "provider quota exhausted",
            LlmErrorKind::RateLimited { .. } => "rate limited by provider",
            LlmErrorKind::ContextOverflow { .. } => "context window exceeded",
            LlmErrorKind::ModelNotFound { .. } => "model not found",
            LlmErrorKind::ServerError { .. } => "provider server error",
            LlmErrorKind::Network => "network error",
            LlmErrorKind::Timeout => "request timed out",
            LlmErrorKind::InvalidRequest { .. } => "invalid request",
            LlmErrorKind::ContentFiltered => "content filtered by provider",
            LlmErrorKind::StreamError => "stream error",
            LlmErrorKind::Provider { .. } => "provider error",
        };
        write!(f, "{prefix}: {summary} — {}", self.message)
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
    fn should_classify_403_without_quota_marker_as_auth() {
        let err = LlmError::from_status(403, "Forbidden");
        assert_eq!(err.kind, LlmErrorKind::Authentication);
    }

    #[test]
    fn should_classify_403_with_quota_marker_as_quota() {
        // The exact body that surfaced on dspfac as "variant=internal
        // recovery=bug": Wisemodel returned 403 with an insufficient_quota
        // payload. We now map that to LlmErrorKind::Quota so the harness
        // taxonomy can route it to a user-friendly message.
        let body = r#"{"error":{"code":"no_active_wisemodel_package","message":"没有有效的 Wisemodel 资源包，请购买后再使用","type":"insufficient_quota"}}"#;
        let err = LlmError::from_status_with_label(403, body, "MiniMax-M2.5-highspeed");
        assert_eq!(err.kind, LlmErrorKind::Quota);
        assert_eq!(err.provider, "MiniMax-M2.5-highspeed");
        // Display surfaces the provider label so operators see which lane
        // is the culprit.
        let rendered = err.to_string();
        assert!(rendered.contains("MiniMax-M2.5-highspeed"));
        assert!(rendered.contains("quota exhausted"));
    }

    #[test]
    fn should_recognise_generic_quota_keyword() {
        let body = r#"{"error":{"type":"insufficient_quota","message":"out of credits"}}"#;
        let err = LlmError::from_status(403, body);
        assert_eq!(err.kind, LlmErrorKind::Quota);
    }

    #[test]
    fn should_recognise_quota_exceeded_keyword() {
        let body = r#"{"error":"quota_exceeded"}"#;
        let err = LlmError::from_status(403, body);
        assert_eq!(err.kind, LlmErrorKind::Quota);
    }

    #[test]
    fn should_classify_429_as_rate_limited() {
        let err = LlmError::from_status(429, "Too Many Requests");
        assert!(matches!(err.kind, LlmErrorKind::RateLimited { .. }));
        assert!(err.is_retryable());
    }

    // ──────────────────────────────────────────────────────────────────────
    // Codex round-2 MAJOR 1: 429 quota disambiguation. 429 is overloaded
    // — providers use it for both throttling and exhausted billing. The
    // body markers tell them apart.
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn should_classify_429_with_insufficient_quota_as_quota() {
        // OpenAI billing-tier exhausted comes back as 429 +
        // `insufficient_quota` (distinct from a transient TPM 429).
        let body = r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota","type":"insufficient_quota"}}"#;
        let err = LlmError::from_status_with_label(429, body, "openai/gpt-4");
        assert_eq!(err.kind, LlmErrorKind::Quota);
        assert!(!err.is_retryable());
    }

    #[test]
    fn should_classify_429_with_bare_resource_exhausted_as_rate_limited() {
        // Codex round-3 MAJOR narrowing: bare `RESOURCE_EXHAUSTED`
        // (without a billing keyword) is ambiguous between billing
        // exhaustion and transient capacity. We now classify it as
        // `RateLimited` so the same-provider backoff can drain capacity
        // blips, and only escalate to `Quota` when the body explicitly
        // names a billing/package marker.
        let body = r#"{"error":{"code":429,"message":"Resource has been exhausted (e.g. check quota)","status":"RESOURCE_EXHAUSTED"}}"#;
        let err = LlmError::from_status_with_label(429, body, "gemini-1.5-pro");
        assert!(matches!(err.kind, LlmErrorKind::RateLimited { .. }));
        assert!(err.is_retryable());
    }

    #[test]
    fn should_classify_429_with_resource_exhausted_plus_billing_as_quota() {
        // Codex round-3 MAJOR narrowing companion: when
        // `RESOURCE_EXHAUSTED` is accompanied by a billing-class keyword
        // (`billing`, `monthly`, `spend`, `package`, `credit`), we
        // upgrade the classification to `Quota` so the failover ladder
        // can move to a different account/lane rather than burn cycles
        // retrying the same exhausted billing tier.
        let body = r#"{"error":{"code":429,"message":"Billing quota exhausted for project","status":"RESOURCE_EXHAUSTED"}}"#;
        let err = LlmError::from_status_with_label(429, body, "gemini-1.5-pro");
        assert_eq!(err.kind, LlmErrorKind::Quota);
    }

    #[test]
    fn should_classify_429_with_no_active_package_as_quota() {
        // Wisemodel returns 429 + `no_active_*_package` when the user's
        // resource pack is exhausted (mirroring the 403 surface).
        let body =
            r#"{"error":{"code":"no_active_wisemodel_package","type":"insufficient_quota"}}"#;
        let err = LlmError::from_status_with_label(429, body, "MiniMax-M2.5-highspeed");
        assert_eq!(err.kind, LlmErrorKind::Quota);
    }

    #[test]
    fn should_classify_429_with_anthropic_rate_limit_error_as_rate_limited() {
        // Anthropic uses 429 + `{"type": "rate_limit_error"}` for real
        // transient throttling (not billing exhaustion).
        let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed your rate limit"}}"#;
        let err = LlmError::from_status_with_label(429, body, "anthropic/claude-3-5-sonnet");
        assert!(matches!(err.kind, LlmErrorKind::RateLimited { .. }));
        assert!(err.is_retryable());
    }

    #[test]
    fn should_classify_402_as_quota() {
        // Anthropic returns 402 for billing failures / inactive subscription.
        let err = LlmError::from_status(402, "Payment Required");
        assert_eq!(err.kind, LlmErrorKind::Quota);
        assert!(!err.is_retryable());
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
    fn should_classify_400_invalid_request_error_payload() {
        // OpenAI-style 400 with explicit invalid_request_error type — must
        // map to InvalidRequest (not ContextOverflow) for the harness to
        // surface a user-friendly "provider rejected the request" message.
        let body = r#"{"error":{"type":"invalid_request_error","message":"unknown parameter: temperature"}}"#;
        let err = LlmError::from_status(400, body);
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
        assert!(s.contains("authentication failed"));
        assert!(s.contains("invalid API key"));
    }

    #[test]
    fn should_display_error_with_provider_label() {
        let err = LlmError::from_status_with_label(403, "Forbidden", "anthropic-vertex");
        let s = err.to_string();
        // Operator log line should identify the provider/model lane.
        assert!(s.contains("anthropic-vertex"));
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
