//! Structured harness error taxonomy (M6.1, issue #488).
//!
//! Classifies every failure that escapes the agent loop into a finite set of
//! variants, each with exactly one primary `RecoveryHint`. Unlike hermes's
//! 809-LOC Python regex classifier, this is a deterministic enum-driven
//! dispatcher that composes with the existing `harness_events` + `abi_schema`
//! + metrics infrastructure.
//!
//! Invariants (per issue #488):
//!   1. No raw `eyre::Report` escapes the agent loop without classification.
//!      Callers convert reports via [`HarnessError::classify_report`].
//!   2. Classification is deterministic — identical input maps to the same
//!      variant and `RecoveryHint` across every invocation.
//!   3. Each variant has exactly one primary `RecoveryHint`.
//!   4. Events emitted via [`HarnessError::to_event`] conform to
//!      `octos.harness.event.v1` and round-trip through
//!      `HarnessEvent::from_json_line`.
//!   5. The schema version is registered as
//!      [`abi_schema::HARNESS_ERROR_SCHEMA_VERSION`].
//!
//! M6.7 note: [`HarnessError::DelegateDepthExceeded`] is reserved for the
//! delegation-depth enforcement task so that variant naming stays consistent
//! across the M6 series.

use std::collections::HashMap;
use std::fmt;

use metrics::counter;
use octos_core::truncated_utf8;
use octos_llm::{LlmError, LlmErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::abi_schema::HARNESS_ERROR_SCHEMA_VERSION;
use crate::harness_events::{HARNESS_EVENT_SCHEMA_V1, HarnessEvent, HarnessEventPayload};

const MAX_HARNESS_ERROR_MESSAGE_BYTES: usize = 1024;
/// Prometheus counter name for loop-level harness errors. Labels:
/// `{variant, recovery}` — both are stable snake_case identifiers.
pub const OCTOS_LOOP_ERROR_TOTAL: &str = "octos_loop_error_total";

/// Recommended recovery action for a `HarnessError`. Each variant maps to
/// exactly one hint — we surface it to the dashboard and the retry layer can
/// use it as a deterministic dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryHint {
    /// Transient failure — retry with exponential backoff (rate limit, network,
    /// timeout).
    BackoffRetry,
    /// Provider-side failure — the retry layer should switch lanes (fallback
    /// provider, alternate model).
    SwitchProvider,
    /// Prompt or conversation history too large — compact context before the
    /// next call.
    CompactContext,
    /// Non-retryable — surface to operator. Includes auth errors, invalid
    /// requests, content filtered responses, and structural budget violations
    /// like delegation depth exceeded.
    FailFast,
    /// Expected policy behaviour, not a fault — e.g. a lifecycle hook denied
    /// the call (#2249). Surface for audit, never page, never retry.
    Expected,
    /// Internal invariant violation — log and bail out; operators must
    /// investigate. Not retryable.
    Bug,
}

impl RecoveryHint {
    /// Stable snake_case identifier used in Prometheus labels and JSON
    /// payloads. Never returns operator-supplied text.
    pub fn as_str(self) -> &'static str {
        match self {
            RecoveryHint::BackoffRetry => "backoff_retry",
            RecoveryHint::SwitchProvider => "switch_provider",
            RecoveryHint::CompactContext => "compact_context",
            RecoveryHint::FailFast => "fail_fast",
            RecoveryHint::Expected => "expected",
            RecoveryHint::Bug => "bug",
        }
    }
}

impl fmt::Display for RecoveryHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed classification of runtime failures the agent loop can hit.
///
/// Every variant carries a short diagnostic `message`; variants that own
/// additional structured context (HTTP status, retry_after hint, limits,
/// tool_name) expose them as named fields. Variant naming is frozen — other
/// milestones reference these names through `variant_name()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessError {
    /// Rate limited by an LLM provider (HTTP 429). Retry after the optional
    /// `retry_after_secs` hint.
    RateLimited {
        retry_after_secs: Option<u64>,
        message: String,
    },
    /// Request exceeded the provider's context window. Callers must compact
    /// history and retry; a raw retry will keep failing.
    ContextOverflow {
        limit: Option<u32>,
        used: Option<u32>,
        message: String,
    },
    /// Authentication failed (HTTP 401, or 403 without a quota marker).
    /// Never retry — surface to operator with "check your API key".
    Authentication { message: String },
    /// Provider quota / billing-tier exhausted (HTTP 403 with
    /// `insufficient_quota` / `quota_exceeded` / `no_active_*_package`).
    /// Distinct from `Authentication` so the operator sees "top up or
    /// switch provider" instead of "bad API key". Never retry — auto-retry
    /// will keep failing until the operator acts.
    Quota { message: String },
    /// The request itself was malformed (HTTP 400, non-context) — bad
    /// parameters, invalid schema, etc.
    InvalidRequest { detail: String, message: String },
    /// Response was blocked by the provider's safety filter.
    ContentFiltered { message: String },
    /// Provider returned 5xx / the stream broke / the model is unavailable —
    /// the retry layer should switch providers.
    ProviderUnavailable {
        status: Option<u16>,
        message: String,
    },
    /// Network-level failure (DNS, TCP reset, TLS error).
    Network { message: String },
    /// Request timed out (client-side or provider-side).
    Timeout { message: String },
    /// A tool's `execute` returned `Err(...)` or panicked. Distinct from
    /// plugin spawn failures so the dashboard can separate fault domains.
    ToolExecution { tool_name: String, message: String },
    /// A plugin executable could not be spawned (missing binary, exec denied).
    PluginSpawn {
        plugin_name: String,
        message: String,
    },
    /// A plugin process exceeded its execution timeout.
    PluginTimeout {
        plugin_name: String,
        timeout_secs: u64,
        message: String,
    },
    /// A plugin returned malformed stdout / violated the binary protocol.
    PluginProtocol {
        plugin_name: String,
        message: String,
    },
    /// Reserved for M6.7 — the spawn/delegate chain exceeded its configured
    /// depth limit. Included here so variant naming stays stable across M6.x
    /// milestones.
    DelegateDepthExceeded {
        depth: u32,
        limit: u32,
        message: String,
    },
    /// A lifecycle hook denied the operation (#2249). Expected policy
    /// behaviour, not a harness fault — classified apart from `Internal` so
    /// operator dashboards and alerting do not treat a policy decision as a
    /// bug.
    PolicyDeny { message: String },
    /// Catch-all for agent-internal bugs (poisoned locks, unexpected state,
    /// etc.). Treat as `RecoveryHint::Bug`.
    Internal { message: String },
}

/// Structured payload body for `HarnessEventPayload::Error`. Serialized as
/// part of `octos.harness.event.v1`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessErrorEvent {
    /// Schema version of this payload — registered in
    /// [`abi_schema::HARNESS_ERROR_SCHEMA_VERSION`].
    #[serde(default = "default_harness_error_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Snake_case variant identifier — matches
    /// [`HarnessError::variant_name`] and is used as the `variant` label on
    /// `octos_loop_error_total`.
    pub variant: String,
    /// Snake_case recovery hint — matches [`RecoveryHint::as_str`].
    pub recovery: String,
    /// Human-readable diagnostic (truncated to 1 KiB).
    pub message: String,
    /// Extra structured context (retry_after, limits, tool_name, etc.).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub details: HashMap<String, Value>,
}

fn default_harness_error_schema_version() -> u32 {
    HARNESS_ERROR_SCHEMA_VERSION
}

impl HarnessError {
    /// Stable snake_case identifier for this variant. Used in Prometheus
    /// labels and the `variant` field of
    /// [`HarnessEventPayload::Error`].
    pub fn variant_name(&self) -> &'static str {
        match self {
            HarnessError::RateLimited { .. } => "rate_limited",
            HarnessError::ContextOverflow { .. } => "context_overflow",
            HarnessError::Authentication { .. } => "authentication",
            HarnessError::Quota { .. } => "quota",
            HarnessError::InvalidRequest { .. } => "invalid_request",
            HarnessError::ContentFiltered { .. } => "content_filtered",
            HarnessError::ProviderUnavailable { .. } => "provider_unavailable",
            HarnessError::Network { .. } => "network",
            HarnessError::Timeout { .. } => "timeout",
            HarnessError::ToolExecution { .. } => "tool_execution",
            HarnessError::PluginSpawn { .. } => "plugin_spawn",
            HarnessError::PluginTimeout { .. } => "plugin_timeout",
            HarnessError::PluginProtocol { .. } => "plugin_protocol",
            HarnessError::DelegateDepthExceeded { .. } => "delegate_depth_exceeded",
            HarnessError::PolicyDeny { .. } => "policy",
            HarnessError::Internal { .. } => "internal",
        }
    }

    /// True for variants raised INSIDE a tool-call batch (`tool_execution`,
    /// `plugin_*`). The agent loop converts these into a tool-result message
    /// and CONTINUES the task (see `execution.rs`: the error is fed back to
    /// the model as feedback), so their `fail_fast` recovery hint scopes to
    /// the individual tool CALL — "don't retry this call" — not to the task.
    /// Observers bridging harness events into task lifecycle (the sink →
    /// `TaskSupervisor::apply_harness_event` path) must NOT treat these as
    /// task-terminal; the owner join path reports the true terminal state.
    ///
    /// Takes the serialized `variant` string (from
    /// [`HarnessErrorEvent::variant`](crate::harness_events::HarnessErrorEvent))
    /// so consumers of persisted/wire events can classify without
    /// reconstructing a `HarnessError`.
    pub fn variant_is_tool_scoped(variant: &str) -> bool {
        matches!(
            variant,
            "tool_execution" | "plugin_spawn" | "plugin_timeout" | "plugin_protocol"
        )
    }

    /// Primary recovery hint for this variant. Each variant maps to exactly
    /// one hint — this is an invariant, not a heuristic.
    pub fn recovery_hint(&self) -> RecoveryHint {
        match self {
            // Transient — exponential backoff retries.
            HarnessError::RateLimited { .. }
            | HarnessError::Network { .. }
            | HarnessError::Timeout { .. }
            | HarnessError::PluginTimeout { .. } => RecoveryHint::BackoffRetry,

            // Provider-side — the retry layer should try a fallback provider.
            HarnessError::ProviderUnavailable { .. } => RecoveryHint::SwitchProvider,

            // Conversation too large — compaction, not retries, unblocks it.
            HarnessError::ContextOverflow { .. } => RecoveryHint::CompactContext,

            // #27b — quota exhaustion (HTTP 402 / 429-with-billing-marker /
            // 403-with-quota-marker) is PROVIDER-DEGRADED, not fatal: the
            // provider's billing window is dead for hours-to-days, but the
            // configured ProviderChain may hold a healthy fallback lane
            // (profile `llm.fallbacks`). Live evidence (2026-08-26/27): k3's
            // 7-day-window quota burn surfaced as `variant=authentication
            // recovery=fail_fast`, the chain's fallback slot never fired, and
            // the only recovery was hand-editing profile JSON. The chain
            // already exists (`build_adaptive_provider_chain`); the missing
            // piece was this recovery hint. `Authentication` (a real 401) is
            // a DIFFERENT failure — a wrong key fails on every lane, so it
            // STAYS FailFast (pinned contract, see
            // `quota_switches_provider_but_401_stays_fail_fast`).
            HarnessError::Quota { .. } => RecoveryHint::SwitchProvider,

            // Non-retryable — surface to operator. #27b red line: a TRUE 401
            // (invalid/expired key) fails identically on every fallback lane,
            // so its FailFast contract must NOT loosen.
            HarnessError::Authentication { .. }
            | HarnessError::InvalidRequest { .. }
            | HarnessError::ContentFiltered { .. }
            | HarnessError::DelegateDepthExceeded { .. }
            | HarnessError::ToolExecution { .. }
            | HarnessError::PluginSpawn { .. }
            | HarnessError::PluginProtocol { .. } => RecoveryHint::FailFast,

            // Non-retryable, but NOT a fault: a lifecycle hook denied the
            // call (#2249). Surface for audit; never page, never retry.
            HarnessError::PolicyDeny { .. } => RecoveryHint::Expected,

            // Internal invariant broken — bug, not recoverable.
            HarnessError::Internal { .. } => RecoveryHint::Bug,
        }
    }

    /// Human-readable diagnostic message (truncated to 1 KiB for storage in
    /// sink events).
    pub fn message(&self) -> &str {
        match self {
            HarnessError::RateLimited { message, .. }
            | HarnessError::ContextOverflow { message, .. }
            | HarnessError::Authentication { message }
            | HarnessError::Quota { message }
            | HarnessError::InvalidRequest { message, .. }
            | HarnessError::ContentFiltered { message }
            | HarnessError::ProviderUnavailable { message, .. }
            | HarnessError::Network { message }
            | HarnessError::Timeout { message }
            | HarnessError::ToolExecution { message, .. }
            | HarnessError::PluginSpawn { message, .. }
            | HarnessError::PluginTimeout { message, .. }
            | HarnessError::PluginProtocol { message, .. }
            | HarnessError::DelegateDepthExceeded { message, .. }
            | HarnessError::PolicyDeny { message }
            | HarnessError::Internal { message } => message,
        }
    }

    /// Prometheus label tuple `(variant, recovery)` for
    /// `octos_loop_error_total`.
    pub fn metric_labels(&self) -> (&'static str, &'static str) {
        (self.variant_name(), self.recovery_hint().as_str())
    }

    /// Increment the `octos_loop_error_total{variant, recovery}` counter for
    /// this error. Callers should invoke this once per classification at the
    /// loop boundary — emitting the event (via [`Self::to_event`]) is a
    /// separate step because sinks are per-task, whereas metrics are global.
    pub fn record_metric(&self) {
        let (variant, recovery) = self.metric_labels();
        counter!(
            OCTOS_LOOP_ERROR_TOTAL,
            "variant" => variant,
            "recovery" => recovery,
        )
        .increment(1);
    }

    /// Classify a raw `eyre::Report` at an agent-loop boundary. Downcasts to
    /// `LlmError` first, then to [`crate::hooks::HookDeniedError`] (#2249 — a
    /// hook deny is policy, not a bug); falls back to `ToolExecution` with
    /// the provided `tool_name`, or `Internal` when no tool context is
    /// available.
    ///
    /// This is the canonical entry point that enforces invariant #1 ("no raw
    /// `eyre::Report` escapes the agent loop without classification"): every
    /// `Err(report)` at an error boundary must be passed through here.
    pub fn classify_report(report: &eyre::Report, tool_name: Option<&str>) -> Self {
        if let Some(llm) = report.downcast_ref::<LlmError>() {
            return Self::from_llm_error(llm);
        }
        // Request construction can fail before a stream exists. Preserve the
        // typed reqwest cause instead of classifying a read/connect deadline
        // hidden by `wrap_err("failed to send streaming request")` as a bug.
        if tool_name.is_none() {
            if let Some(network) = report
                .chain()
                .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
            {
                let message = truncate(&report.to_string(), MAX_HARNESS_ERROR_MESSAGE_BYTES);
                if network.is_timeout() {
                    return HarnessError::Timeout { message };
                }
                if network.status().is_none()
                    && (network.is_connect() || network.is_request() || network.is_body())
                {
                    return HarnessError::Network { message };
                }
            }
        }
        // #2249 — a before-hook deny is expected policy behaviour: classify it
        // `policy`/`expected` so dashboards stop paging on it as `internal`/`bug`.
        if report
            .downcast_ref::<crate::hooks::HookDeniedError>()
            .is_some()
        {
            return HarnessError::PolicyDeny {
                message: truncate(&report.to_string(), MAX_HARNESS_ERROR_MESSAGE_BYTES),
            };
        }
        let message = truncate(&report.to_string(), MAX_HARNESS_ERROR_MESSAGE_BYTES);
        match tool_name {
            Some(name) => HarnessError::ToolExecution {
                tool_name: name.to_string(),
                message,
            },
            None => HarnessError::Internal { message },
        }
    }

    /// Classify an owned `LlmError` borrow without consuming it. Public so
    /// callers can opportunistically classify in diagnostic paths.
    ///
    /// The user-facing messages embedded here are what the SPA renders when
    /// no specific variant handling exists on the client — they MUST be
    /// actionable ("check API key", "top up provider", "switch lane") and
    /// MUST identify the provider lane that failed so operators can pivot
    /// without digging through daemon logs.
    pub fn from_llm_error(err: &LlmError) -> Self {
        let raw = truncate(&err.message, MAX_HARNESS_ERROR_MESSAGE_BYTES);
        // Identify the provider lane in user-facing messages. Empty label
        // is rendered as a generic "provider" so the message still parses.
        let lane = if err.provider.is_empty() {
            "provider".to_string()
        } else {
            err.provider.clone()
        };
        match &err.kind {
            LlmErrorKind::Authentication => HarnessError::Authentication {
                message: format!("Authentication failed for {lane} — check your API key ({raw})"),
            },
            LlmErrorKind::Quota => HarnessError::Quota {
                message: format!(
                    "Provider quota exhausted ({lane}) — top up or switch provider ({raw})"
                ),
            },
            LlmErrorKind::RateLimited { retry_after_secs } => HarnessError::RateLimited {
                retry_after_secs: *retry_after_secs,
                message: format!("{lane} rate-limited — backing off ({raw})"),
            },
            LlmErrorKind::ContextOverflow { limit, used } => HarnessError::ContextOverflow {
                limit: *limit,
                used: *used,
                message: raw,
            },
            LlmErrorKind::ModelNotFound { model } => HarnessError::ProviderUnavailable {
                status: Some(404),
                message: format!("model not found on {lane}: {model}"),
            },
            LlmErrorKind::ServerError { status } => HarnessError::ProviderUnavailable {
                status: Some(*status),
                message: format!("{lane} temporarily unavailable ({raw})"),
            },
            LlmErrorKind::Network => HarnessError::Network { message: raw },
            LlmErrorKind::Timeout => HarnessError::Timeout { message: raw },
            LlmErrorKind::InvalidRequest { detail } => HarnessError::InvalidRequest {
                detail: truncate(detail, MAX_HARNESS_ERROR_MESSAGE_BYTES),
                message: format!(
                    "Provider rejected the request ({lane}): {raw}. This often means a model/feature mismatch."
                ),
            },
            LlmErrorKind::ContentFiltered => HarnessError::ContentFiltered { message: raw },
            LlmErrorKind::StreamError => HarnessError::ProviderUnavailable {
                status: None,
                message: format!("{lane} stream broke ({raw})"),
            },
            LlmErrorKind::Provider { .. } => HarnessError::ProviderUnavailable {
                status: None,
                message: format!("{lane} returned an error ({raw})"),
            },
        }
    }

    /// Build a `HarnessEvent` carrying this error. The emitted payload is a
    /// valid `octos.harness.event.v1` record and round-trips through
    /// `HarnessEvent::from_json_line`.
    pub fn to_event(
        &self,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<&str>,
        phase: Option<&str>,
    ) -> HarnessEvent {
        HarnessEvent {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::Error {
                data: self.to_event_body(session_id, task_id, workflow, phase),
            },
        }
    }

    /// Build just the payload body — used by the `From<HarnessError>` impl
    /// for [`HarnessEventPayload`] and internally by [`Self::to_event`].
    pub fn to_event_body(
        &self,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<&str>,
        phase: Option<&str>,
    ) -> HarnessErrorEvent {
        HarnessErrorEvent {
            schema_version: HARNESS_ERROR_SCHEMA_VERSION,
            session_id: session_id.into(),
            task_id: task_id.into(),
            workflow: workflow.map(ToOwned::to_owned),
            phase: phase.map(ToOwned::to_owned),
            variant: self.variant_name().to_string(),
            recovery: self.recovery_hint().as_str().to_string(),
            message: truncate(self.message(), MAX_HARNESS_ERROR_MESSAGE_BYTES),
            details: self.details(),
        }
    }

    fn details(&self) -> HashMap<String, Value> {
        let mut out = HashMap::new();
        match self {
            HarnessError::RateLimited {
                retry_after_secs, ..
            } => {
                if let Some(secs) = retry_after_secs {
                    out.insert("retry_after_secs".into(), Value::from(*secs));
                }
            }
            HarnessError::ContextOverflow { limit, used, .. } => {
                if let Some(limit) = limit {
                    out.insert("limit".into(), Value::from(*limit));
                }
                if let Some(used) = used {
                    out.insert("used".into(), Value::from(*used));
                }
            }
            HarnessError::ProviderUnavailable { status, .. } => {
                if let Some(status) = status {
                    out.insert("status".into(), Value::from(*status));
                }
            }
            HarnessError::InvalidRequest { detail, .. } => {
                out.insert("detail".into(), Value::from(detail.clone()));
            }
            HarnessError::ToolExecution { tool_name, .. } => {
                out.insert("tool_name".into(), Value::from(tool_name.clone()));
            }
            HarnessError::PluginSpawn { plugin_name, .. }
            | HarnessError::PluginProtocol { plugin_name, .. } => {
                out.insert("plugin_name".into(), Value::from(plugin_name.clone()));
            }
            HarnessError::PluginTimeout {
                plugin_name,
                timeout_secs,
                ..
            } => {
                out.insert("plugin_name".into(), Value::from(plugin_name.clone()));
                out.insert("timeout_secs".into(), Value::from(*timeout_secs));
            }
            HarnessError::DelegateDepthExceeded { depth, limit, .. } => {
                out.insert("depth".into(), Value::from(*depth));
                out.insert("limit".into(), Value::from(*limit));
            }
            HarnessError::Authentication { .. }
            | HarnessError::Quota { .. }
            | HarnessError::ContentFiltered { .. }
            | HarnessError::Network { .. }
            | HarnessError::Timeout { .. }
            | HarnessError::PolicyDeny { .. }
            | HarnessError::Internal { .. } => {}
        }
        out
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.variant_name(),
            self.recovery_hint().as_str(),
            self.message()
        )
    }
}

impl std::error::Error for HarnessError {}

impl From<LlmError> for HarnessError {
    fn from(err: LlmError) -> Self {
        HarnessError::from_llm_error(&err)
    }
}

impl From<HarnessError> for HarnessEventPayload {
    fn from(err: HarnessError) -> Self {
        // Without a concrete session/task, we default to "unknown" IDs — the
        // loop-boundary helper always provides real IDs, but this impl exists
        // so the enum composes with the `HarnessEventPayload` public API.
        HarnessEventPayload::Error {
            data: err.to_event_body("unknown", "unknown", None, None),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    truncated_utf8(s, max, "…")
}
