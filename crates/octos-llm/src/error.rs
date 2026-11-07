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
    /// Wire protocol of the lane, rendered in `Display` as `api_style=…` so
    /// status-code errors name the concrete lane the same way transport and
    /// operational errors do. `provider` itself is unchanged for consumers.
    pub api_style: Option<crate::provider::ApiStyle>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl LlmError {
    pub fn new(kind: LlmErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            provider: String::new(),
            api_style: None,
            source: None,
        }
    }

    /// Builder: attach the operator-visible provider/model label.
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// Builder: attach the lane's wire protocol (`api_style=…` in `Display`).
    pub fn with_api_style(mut self, api_style: crate::provider::ApiStyle) -> Self {
        self.api_style = Some(api_style);
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
            api_style: None,
            source: None,
        }
    }
}

impl fmt::Display for LlmError {
    /// Human-readable rendering. Provider label is prefixed when non-empty
    /// so the operator sees which lane failed (e.g.
    /// "API error (MiniMax-M2.5-highspeed): provider quota exhausted — HTTP 403 - ...").
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match (self.provider.is_empty(), self.api_style) {
            (true, None) => "API error".to_string(),
            (true, Some(style)) => format!("API error (api_style={style})"),
            (false, None) => format!("API error ({})", self.provider),
            (false, Some(style)) => format!("API error ({}, api_style={style})", self.provider),
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

/// Typed errors produced by the LLM streaming layer.
///
/// Codex round (#1355): the streaming layer used to silently break its read
/// loop on inter-chunk timeout with a half-assembled `tool_call.arguments`
/// buffer, then ship a sentinel `Value::String("MALFORMED_JSON:...")`
/// downstream as if it were a valid `ChatResponse`. The plugin executor
/// dispatched the sentinel string and errored with "missing 'out'". See
/// `docs/STREAMING-TRANSACTIONAL-BOUNDARY-ADR.md` (PR #1355) for the full
/// architectural argument.
///
/// `StreamError` is the typed boundary the agent-loop side of the streaming
/// layer surfaces instead of silently propagating partial state. The variants
/// implement the contract:
///
/// * `IdleTimeout` and `Transport` are retryable — the existing
///   `RetryProvider` / `ProviderChain` machinery handles them as it does any
///   other transient stream failure.
/// * `Incomplete` (stream closed without `finish_reason`) is treated as
///   retryable so the lane router can pick a different slot if the
///   wire-shape mismatch repeats.
/// * `MalformedArgs` is **not** retryable. The model produced syntactically
///   bad JSON for a tool_call after a clean `finish_reason` — silently
///   retrying would hide the bug from the model so it can't self-correct.
///   The error reaches the loop, gets surfaced as a tool-failure, and the
///   model sees the diagnostic on its next turn.
#[derive(Debug)]
pub enum StreamError {
    /// Inter-chunk idle timeout fired during streaming — provider stalled.
    /// Half-assembled buffers are discarded; the caller should retry from
    /// scratch via the existing retry machinery.
    IdleTimeout {
        /// Idle window that elapsed without a chunk arriving.
        idle_secs: u64,
    },
    /// Stream closed without a terminal signal (`finish_reason`).
    Incomplete {
        /// Operator-visible context (e.g. "stream ended before Done event").
        detail: String,
    },
    /// Tool call arrived with un-parseable JSON arguments AFTER a clean
    /// terminal signal. This is a model-side bug; surfacing it as
    /// non-retryable lets the agent loop give the model a chance to
    /// self-correct on its next turn.
    MalformedArgs {
        /// Tool call id (provider-assigned).
        tool_id: String,
        /// Tool name as declared in the stream.
        tool_name: String,
        /// Parse error rendered with truncated raw input for log brevity.
        error: String,
    },
    /// Tool call arguments failed to parse because the turn hit the output
    /// token cap (`finish_reason=length` → [`StopReason::MaxTokens`]) mid-call,
    /// truncating the JSON. Unlike [`StreamError::MalformedArgs`], this is NOT a
    /// model bug — the model was cut off, not wrong — so it is **retryable**:
    /// the existing retry machinery re-requests the turn (reasoning-length
    /// variance and the non-streaming fallback often complete the call on a
    /// second attempt). Distinguishing this from `MalformedArgs` preserves the
    /// #1355 invariant (a syntactically bad call after a *clean* finish stays
    /// non-retryable so the model self-corrects) while stopping a truncated
    /// call from instantly killing a background task, which has no "next turn".
    TruncatedToolCall {
        /// Tool call id (provider-assigned or synthesized).
        tool_id: String,
        /// Tool name as declared in the stream.
        tool_name: String,
        /// Parse error rendered with truncated raw input for log brevity.
        error: String,
    },
    /// Underlying transport failure (connection reset, broken pipe, etc.).
    Transport { detail: String },
}

impl StreamError {
    /// Whether the existing retry machinery should treat this error as
    /// transient and retry the stream. See the variant docs for policy.
    pub fn is_retryable(&self) -> bool {
        match self {
            StreamError::IdleTimeout { .. }
            | StreamError::Incomplete { .. }
            | StreamError::TruncatedToolCall { .. }
            | StreamError::Transport { .. } => true,
            StreamError::MalformedArgs { .. } => false,
        }
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::IdleTimeout { idle_secs } => {
                write!(
                    f,
                    "stream idle timeout after {idle_secs}s — provider stalled"
                )
            }
            StreamError::Incomplete { detail } => {
                write!(f, "stream closed without terminal signal: {detail}")
            }
            StreamError::MalformedArgs {
                tool_id,
                tool_name,
                error,
            } => {
                write!(
                    f,
                    "malformed tool_call arguments (tool={tool_name}, id={tool_id}): {error}"
                )
            }
            StreamError::TruncatedToolCall {
                tool_id,
                tool_name,
                error,
            } => {
                write!(
                    f,
                    "truncated tool_call arguments (tool={tool_name}, id={tool_id}) — output token cap hit mid-call: {error}"
                )
            }
            StreamError::Transport { detail } => write!(f, "stream transport error: {detail}"),
        }
    }
}

impl std::error::Error for StreamError {}

impl From<StreamError> for LlmError {
    /// Bridge `StreamError` into the existing `LlmError` taxonomy so the
    /// failover/retry plumbing handles it uniformly. The mapping mirrors
    /// the retryability policy on `StreamError::is_retryable()`:
    ///
    /// * `IdleTimeout` → `Timeout` (retryable in `LlmError::is_retryable`)
    /// * `Incomplete` → `StreamError` (retryable)
    /// * `MalformedArgs` → `InvalidRequest` (NOT retryable — model needs to see)
    /// * `TruncatedToolCall` → `StreamError` (retryable — cut off, not wrong)
    /// * `Transport` → `Network` (retryable)
    fn from(err: StreamError) -> Self {
        let message = err.to_string();
        let kind = match &err {
            StreamError::IdleTimeout { .. } => LlmErrorKind::Timeout,
            StreamError::Incomplete { .. } => LlmErrorKind::StreamError,
            StreamError::MalformedArgs { .. } => LlmErrorKind::InvalidRequest {
                detail: message.chars().take(200).collect(),
            },
            StreamError::TruncatedToolCall { .. } => LlmErrorKind::StreamError,
            StreamError::Transport { .. } => LlmErrorKind::Network,
        };
        LlmError::new(kind, message).with_source(err)
    }
}

#[cfg(test)]
mod tests {

    // ──────────────────────────────────────────────────────────────────────
    // Codex round-2 MAJOR 1: 429 quota disambiguation. 429 is overloaded
    // — providers use it for both throttling and exhausted billing. The
    // body markers tell them apart.
    // ──────────────────────────────────────────────────────────────────────

    // ──────────────────────────────────────────────────────────────────────
    // StreamError — typed boundary for the streaming layer. See ADR
    // docs/STREAMING-TRANSACTIONAL-BOUNDARY-ADR.md.
    // ──────────────────────────────────────────────────────────────────────
}

#[cfg(test)]
mod api_style_display_tests {}
