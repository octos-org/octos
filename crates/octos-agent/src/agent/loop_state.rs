//! Typed retry-bucket state machine for the agent loop (M6.2, issue #489).
//!
//! This is the decision layer layered on top of M6.1's `HarnessError` taxonomy.
//! Whereas `classify_loop_error` turns raw `eyre::Report` into a typed variant
//! with a primary [`RecoveryHint`], [`LoopRetryState`] decides whether the
//! *next* loop iteration should continue, compact context, rotate the
//! credential/provider lane, escalate, or fire a single free grace call past
//! hard budget. Every decision is deterministic.
//!
//! Design goals (per issue #489):
//!   1. Each [`HarnessError`] variant maps to exactly one typed counter with a
//!      bounded limit. Exhausting a bucket never silently loops.
//!   2. The state survives context compaction via `serde` round-trip.
//!   3. A single "budget-grace-call" may fire past `max_iterations` iff the
//!      loop produced at least one productive tool call since the last grace.
//!   4. The existing shell-spiral recovery (formerly the free-standing
//!      `recover_shell_retry` helper) routes through the same machine via
//!      [`LoopRetryState::observe_shell_spiral`], so behavior matches byte-for-byte.
//!   5. Every observation emits a typed `HarnessEventPayload::Retry` event and
//!      increments `octos_loop_retry_total{variant, decision}`.
//!
//! Invariants enforced in unit tests (see `tests/loop_retry_state.rs`):
//!   - `should_escalate_after_invalid_tool_call_limit`
//!   - `should_compact_on_context_overflow_decision`
//!   - `should_fire_grace_call_at_budget_exhaustion_with_productive_history`
//!   - `should_not_fire_grace_call_without_productive_history`
//!   - `should_serde_round_trip_loop_retry_state`
//!   - `should_preserve_shell_spiral_recovery_behavior`

use std::fmt;

use metrics::counter;
use serde::{Deserialize, Serialize};

use crate::harness_errors::{HarnessError, RecoveryHint};
use crate::harness_events::{
    HARNESS_EVENT_SCHEMA_V1, HarnessEvent, HarnessEventPayload, HarnessRetryEvent,
};

/// Prometheus counter name for loop-level retry decisions. Labels:
/// `{variant, decision}` — both are stable snake_case identifiers.
pub const OCTOS_LOOP_RETRY_TOTAL: &str = "octos_loop_retry_total";

// ── Default per-bucket limits ───────────────────────────────────────────────
//
// Limits are intentionally conservative; each bucket has to trigger `Exhausted`
// strictly before an unbounded runaway sets in. The numbers are tuned so that
// transient failures (network blips, rate-limit bursts) get a few reasonable
// retries while structural problems (auth, malformed tool calls, invalid
// schemas) escalate quickly.

const DEFAULT_RATE_LIMIT_LIMIT: u32 = 5;
const DEFAULT_CONTEXT_OVERFLOW_LIMIT: u32 = 2;
const DEFAULT_AUTHENTICATION_LIMIT: u32 = 1;
const DEFAULT_INVALID_REQUEST_LIMIT: u32 = 2;
const DEFAULT_CONTENT_FILTERED_LIMIT: u32 = 1;
const DEFAULT_PROVIDER_UNAVAILABLE_LIMIT: u32 = 4;
const DEFAULT_NETWORK_LIMIT: u32 = 4;
const DEFAULT_TIMEOUT_LIMIT: u32 = 3;
const DEFAULT_TOOL_EXECUTION_LIMIT: u32 = 5;
const DEFAULT_PLUGIN_SPAWN_LIMIT: u32 = 2;
const DEFAULT_PLUGIN_TIMEOUT_LIMIT: u32 = 3;
const DEFAULT_PLUGIN_PROTOCOL_LIMIT: u32 = 2;
const DEFAULT_DELEGATE_DEPTH_LIMIT: u32 = 1;
const DEFAULT_INTERNAL_LIMIT: u32 = 1;
const DEFAULT_SHELL_SPIRAL_LIMIT: u32 = 1;

/// Per-bucket hard limits. Tuned for M6.2 defaults, exposed so integration
/// tests and operators can override them if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRetryLimits {
    pub rate_limited: u32,
    pub context_overflow: u32,
    pub authentication: u32,
    pub invalid_request: u32,
    pub content_filtered: u32,
    pub provider_unavailable: u32,
    pub network: u32,
    pub timeout: u32,
    pub tool_execution: u32,
    pub plugin_spawn: u32,
    pub plugin_timeout: u32,
    pub plugin_protocol: u32,
    pub delegate_depth_exceeded: u32,
    pub internal: u32,
    pub shell_spiral: u32,
}

impl Default for LoopRetryLimits {
    fn default() -> Self {
        Self {
            rate_limited: DEFAULT_RATE_LIMIT_LIMIT,
            context_overflow: DEFAULT_CONTEXT_OVERFLOW_LIMIT,
            authentication: DEFAULT_AUTHENTICATION_LIMIT,
            invalid_request: DEFAULT_INVALID_REQUEST_LIMIT,
            content_filtered: DEFAULT_CONTENT_FILTERED_LIMIT,
            provider_unavailable: DEFAULT_PROVIDER_UNAVAILABLE_LIMIT,
            network: DEFAULT_NETWORK_LIMIT,
            timeout: DEFAULT_TIMEOUT_LIMIT,
            tool_execution: DEFAULT_TOOL_EXECUTION_LIMIT,
            plugin_spawn: DEFAULT_PLUGIN_SPAWN_LIMIT,
            plugin_timeout: DEFAULT_PLUGIN_TIMEOUT_LIMIT,
            plugin_protocol: DEFAULT_PLUGIN_PROTOCOL_LIMIT,
            delegate_depth_exceeded: DEFAULT_DELEGATE_DEPTH_LIMIT,
            internal: DEFAULT_INTERNAL_LIMIT,
            shell_spiral: DEFAULT_SHELL_SPIRAL_LIMIT,
        }
    }
}

/// The decision the retry layer returns to the agent loop after a failure
/// observation. Each decision has a stable snake_case name used in metrics and
/// structured events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopDecision {
    /// Retry without reshaping the prompt: the underlying failure is expected
    /// to clear on its own (rate limit burst, flaky network, slow tool).
    Continue,
    /// Swap provider/credential lane before the next call. Used for provider
    /// outages (5xx, stream aborts) where the current lane is sick but the
    /// task itself is still valid.
    RotateAndRetry,
    /// Compact the conversation (drop old messages, summarize) and retry. This
    /// is the only viable recovery for `ContextOverflow`.
    CompactAndRetry,
    /// Not retryable here — caller should surface the error and stop. Used
    /// for structural failures (auth, invalid request, content filter,
    /// delegation depth, tool/plugin faults, bugs).
    Escalate,
    /// Bucket exhausted: the same failure happened more times than the
    /// configured limit. Caller must treat this as a hard stop to avoid
    /// silent infinite loops (invariant #2 from #489).
    Exhausted,
    /// One free iteration past the hard iteration budget because the loop
    /// produced at least one productive tool call since the last grace. Once
    /// fired, cannot fire again until another productive call is recorded.
    Grace,
}

impl LoopDecision {
    /// Stable snake_case identifier used in metrics labels and structured
    /// event `message` fields. Never returns operator-supplied text.
    pub fn as_str(self) -> &'static str {
        match self {
            LoopDecision::Continue => "continue",
            LoopDecision::RotateAndRetry => "rotate_and_retry",
            LoopDecision::CompactAndRetry => "compact_and_retry",
            LoopDecision::Escalate => "escalate",
            LoopDecision::Exhausted => "exhausted",
            LoopDecision::Grace => "grace",
        }
    }
}

impl fmt::Display for LoopDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable snake_case identifier for the shell-spiral bucket. The spiral is
/// not a `HarnessError` variant but flows through the same state machine
/// so operators see one coherent retry surface.
pub const SHELL_SPIRAL_VARIANT: &str = "shell_spiral";

/// Per-variant counters. Each counter is bumped exactly once per observation
/// and the corresponding limit from [`LoopRetryLimits`] is checked immediately
/// so the caller never silently exceeds a bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRetryCounters {
    pub rate_limited: u32,
    pub context_overflow: u32,
    pub authentication: u32,
    pub invalid_request: u32,
    pub content_filtered: u32,
    pub provider_unavailable: u32,
    pub network: u32,
    pub timeout: u32,
    pub tool_execution: u32,
    pub plugin_spawn: u32,
    pub plugin_timeout: u32,
    pub plugin_protocol: u32,
    pub delegate_depth_exceeded: u32,
    pub internal: u32,
    pub shell_spiral: u32,
}

/// Loop-level retry state machine. Owns one bounded counter per
/// [`HarnessError`] variant plus the shell-spiral synthetic bucket, and
/// tracks grace-call eligibility.
///
/// The state is entirely `serde`-serializable so that the compaction path
/// can round-trip it through the session ledger; see
/// `should_serde_round_trip_loop_retry_state`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRetryState {
    #[serde(default)]
    pub counters: LoopRetryCounters,
    #[serde(default)]
    pub limits: LoopRetryLimits,
    /// Count of productive tool calls (success=true, non-error) recorded since
    /// the last grace call. Must be ≥ 1 for the next grace call to fire.
    #[serde(default)]
    pub productive_tool_calls_since_last_grace: u32,
    /// Number of grace calls fired so far. Useful for metrics and debugging;
    /// the decision logic only cares about productive_tool_calls_since_last_grace.
    #[serde(default)]
    pub grace_calls_fired: u32,
}

impl LoopRetryState {
    /// Construct a fresh state with the default per-bucket limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a state with explicit limits — useful for tests that need to
    /// drive a bucket to exhaustion quickly without relying on default tuning.
    pub fn with_limits(limits: LoopRetryLimits) -> Self {
        Self {
            counters: LoopRetryCounters::default(),
            limits,
            productive_tool_calls_since_last_grace: 0,
            grace_calls_fired: 0,
        }
    }

    /// Record a productive tool call (one whose `ToolResult.success` was true
    /// and produced meaningful output). Used to gate the grace-call pathway so
    /// that a stalled loop with no productive history does not get extra
    /// iterations past budget.
    pub fn record_productive_tool_call(&mut self) {
        self.productive_tool_calls_since_last_grace = self
            .productive_tool_calls_since_last_grace
            .saturating_add(1);
    }

    /// Classify a failure, bump the matching counter, and return the next
    /// decision. The decision is determined purely by the variant and the
    /// counter vs. limit comparison — it never depends on the error message,
    /// so the result is deterministic for the same variant.
    ///
    /// This is the canonical entry point for the retry layer; callers should
    /// pair it with [`Self::emit_event`] to make the decision observable.
    pub fn observe(&mut self, error: &HarnessError) -> LoopDecision {
        let (count, limit) = self.bump_counter(error);
        let decision = if count > limit {
            LoopDecision::Exhausted
        } else {
            decide_for_variant(error)
        };
        Self::record_metric(error.variant_name(), decision);
        decision
    }

    /// Observe a shell-spiral event (existing `recover_shell_retry` behavior).
    /// The state machine owns the counter so operators see one coherent retry
    /// ledger; the actual spiral detection lives in `loop_runner.rs`.
    ///
    /// Returns [`LoopDecision::Escalate`] on the first spiral hit and
    /// [`LoopDecision::Exhausted`] if the spiral limit is exceeded — either
    /// way the caller must stop retrying shell and surface the latest output.
    pub fn observe_shell_spiral(&mut self) -> LoopDecision {
        self.counters.shell_spiral = self.counters.shell_spiral.saturating_add(1);
        let decision = if self.counters.shell_spiral > self.limits.shell_spiral {
            LoopDecision::Exhausted
        } else {
            LoopDecision::Escalate
        };
        Self::record_metric(SHELL_SPIRAL_VARIANT, decision);
        decision
    }

    /// Resolve the decision at hard-budget exhaustion. Returns
    /// [`LoopDecision::Grace`] iff there has been at least one productive
    /// tool call since the last grace call; otherwise returns
    /// [`LoopDecision::Escalate`].
    ///
    /// A `Grace` decision *consumes* the productive history: subsequent
    /// grace calls require fresh productive tool calls.
    pub fn observe_budget_exhaustion(&mut self) -> LoopDecision {
        let decision = if self.productive_tool_calls_since_last_grace >= 1 {
            self.productive_tool_calls_since_last_grace = 0;
            self.grace_calls_fired = self.grace_calls_fired.saturating_add(1);
            LoopDecision::Grace
        } else {
            LoopDecision::Escalate
        };
        Self::record_metric("budget_exhaustion", decision);
        decision
    }

    /// Snapshot of the current counters — exposed for metrics export and
    /// debugging. Mutations must go through `observe*` or
    /// `record_productive_tool_call`.
    pub fn counters(&self) -> LoopRetryCounters {
        self.counters
    }

    /// Emit a structured `HarnessEventPayload::Retry` event carrying the
    /// variant + decision pair. Returns the constructed event so the caller
    /// can also write it to the local harness event sink without rebuilding it.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_event(
        &self,
        variant: &str,
        decision: LoopDecision,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<&str>,
        phase: Option<&str>,
        attempt: Option<u32>,
    ) -> HarnessEvent {
        HarnessEvent {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::Retry {
                data: HarnessRetryEvent {
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    workflow: workflow.map(ToOwned::to_owned),
                    phase: phase.map(ToOwned::to_owned),
                    attempt,
                    message: Some(
                        format!("variant={variant} decision={} ", decision.as_str())
                            .trim_end()
                            .to_string(),
                    ),
                    extra: {
                        let mut extra = std::collections::HashMap::new();
                        extra.insert("variant".to_string(), serde_json::Value::from(variant));
                        extra.insert(
                            "decision".to_string(),
                            serde_json::Value::from(decision.as_str()),
                        );
                        extra
                    },
                },
            },
        }
    }

    fn bump_counter(&mut self, error: &HarnessError) -> (u32, u32) {
        let (counter_ref, limit) = match error {
            HarnessError::RateLimited { .. } => {
                (&mut self.counters.rate_limited, self.limits.rate_limited)
            }
            HarnessError::ContextOverflow { .. } => (
                &mut self.counters.context_overflow,
                self.limits.context_overflow,
            ),
            HarnessError::Authentication { .. } => (
                &mut self.counters.authentication,
                self.limits.authentication,
            ),
            HarnessError::InvalidRequest { .. } => (
                &mut self.counters.invalid_request,
                self.limits.invalid_request,
            ),
            HarnessError::ContentFiltered { .. } => (
                &mut self.counters.content_filtered,
                self.limits.content_filtered,
            ),
            HarnessError::ProviderUnavailable { .. } => (
                &mut self.counters.provider_unavailable,
                self.limits.provider_unavailable,
            ),
            HarnessError::Network { .. } => (&mut self.counters.network, self.limits.network),
            HarnessError::Timeout { .. } => (&mut self.counters.timeout, self.limits.timeout),
            HarnessError::ToolExecution { .. } => (
                &mut self.counters.tool_execution,
                self.limits.tool_execution,
            ),
            HarnessError::PluginSpawn { .. } => {
                (&mut self.counters.plugin_spawn, self.limits.plugin_spawn)
            }
            HarnessError::PluginTimeout { .. } => (
                &mut self.counters.plugin_timeout,
                self.limits.plugin_timeout,
            ),
            HarnessError::PluginProtocol { .. } => (
                &mut self.counters.plugin_protocol,
                self.limits.plugin_protocol,
            ),
            HarnessError::DelegateDepthExceeded { .. } => (
                &mut self.counters.delegate_depth_exceeded,
                self.limits.delegate_depth_exceeded,
            ),
            HarnessError::Internal { .. } => (&mut self.counters.internal, self.limits.internal),
        };
        *counter_ref = counter_ref.saturating_add(1);
        (*counter_ref, limit)
    }

    fn record_metric(variant: &str, decision: LoopDecision) {
        counter!(
            OCTOS_LOOP_RETRY_TOTAL,
            "variant" => variant.to_string(),
            "decision" => decision.as_str().to_string(),
        )
        .increment(1);
    }
}

/// Map a `HarnessError` variant to the canonical loop decision, ignoring
/// bucket exhaustion. The caller decides whether to return [`LoopDecision::Exhausted`]
/// based on the counter/limit comparison.
fn decide_for_variant(error: &HarnessError) -> LoopDecision {
    match error.recovery_hint() {
        // Transient — retry without reshaping context.
        RecoveryHint::BackoffRetry => LoopDecision::Continue,
        // Provider outage — swap lanes.
        RecoveryHint::SwitchProvider => LoopDecision::RotateAndRetry,
        // Conversation too large — only compaction unblocks it.
        RecoveryHint::CompactContext => LoopDecision::CompactAndRetry,
        // Non-retryable, surface to operator.
        RecoveryHint::FailFast => LoopDecision::Escalate,
        // Internal invariant violation — bug, not recoverable.
        RecoveryHint::Bug => LoopDecision::Escalate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate_limit() -> HarnessError {
        HarnessError::RateLimited {
            retry_after_secs: Some(1),
            message: "429".into(),
        }
    }

    fn context_overflow() -> HarnessError {
        HarnessError::ContextOverflow {
            limit: Some(200_000),
            used: Some(201_000),
            message: "context exceeded".into(),
        }
    }

    fn auth_error() -> HarnessError {
        HarnessError::Authentication {
            message: "bad key".into(),
        }
    }

    fn tool_error() -> HarnessError {
        HarnessError::ToolExecution {
            tool_name: "shell".into(),
            message: "exit 1".into(),
        }
    }

    #[test]
    fn observe_rate_limit_returns_continue_until_limit() {
        let mut state = LoopRetryState::with_limits(LoopRetryLimits {
            rate_limited: 2,
            ..Default::default()
        });
        assert_eq!(state.observe(&rate_limit()), LoopDecision::Continue);
        assert_eq!(state.observe(&rate_limit()), LoopDecision::Continue);
        assert_eq!(state.observe(&rate_limit()), LoopDecision::Exhausted);
    }

    #[test]
    fn observe_context_overflow_returns_compact_then_exhausts() {
        let mut state = LoopRetryState::with_limits(LoopRetryLimits {
            context_overflow: 1,
            ..Default::default()
        });
        assert_eq!(
            state.observe(&context_overflow()),
            LoopDecision::CompactAndRetry
        );
        assert_eq!(state.observe(&context_overflow()), LoopDecision::Exhausted);
    }

    #[test]
    fn observe_authentication_always_escalates() {
        let mut state = LoopRetryState::new();
        assert_eq!(state.observe(&auth_error()), LoopDecision::Escalate);
    }

    #[test]
    fn observe_tool_execution_escalates_up_to_limit() {
        let mut state = LoopRetryState::with_limits(LoopRetryLimits {
            tool_execution: 2,
            ..Default::default()
        });
        // Tool execution errors are FailFast in M6.1's hint table, so the
        // decision is always Escalate until the limit is exhausted.
        assert_eq!(state.observe(&tool_error()), LoopDecision::Escalate);
        assert_eq!(state.observe(&tool_error()), LoopDecision::Escalate);
        assert_eq!(state.observe(&tool_error()), LoopDecision::Exhausted);
    }

    #[test]
    fn grace_call_fires_with_productive_history() {
        let mut state = LoopRetryState::new();
        state.record_productive_tool_call();
        assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Grace);
        assert_eq!(state.grace_calls_fired, 1);
        assert_eq!(state.productive_tool_calls_since_last_grace, 0);
    }

    #[test]
    fn grace_call_escalates_without_productive_history() {
        let mut state = LoopRetryState::new();
        assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Escalate);
        assert_eq!(state.grace_calls_fired, 0);
    }

    #[test]
    fn grace_call_resets_productive_counter() {
        let mut state = LoopRetryState::new();
        state.record_productive_tool_call();
        state.record_productive_tool_call();
        assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Grace);
        // Productive history consumed; second call without fresh productive
        // tool calls must escalate.
        assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Escalate);
    }

    #[test]
    fn shell_spiral_escalates_on_first_hit_then_exhausts() {
        let mut state = LoopRetryState::with_limits(LoopRetryLimits {
            shell_spiral: 1,
            ..Default::default()
        });
        assert_eq!(state.observe_shell_spiral(), LoopDecision::Escalate);
        assert_eq!(state.observe_shell_spiral(), LoopDecision::Exhausted);
    }

    #[test]
    fn serde_round_trips_loop_retry_state() {
        let mut state = LoopRetryState::new();
        state.observe(&rate_limit());
        state.observe(&context_overflow());
        state.record_productive_tool_call();

        let json = serde_json::to_string(&state).expect("serialize");
        let restored: LoopRetryState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, restored);
    }

    #[test]
    fn emit_event_builds_valid_retry_payload() {
        let state = LoopRetryState::new();
        let event = state.emit_event(
            "rate_limited",
            LoopDecision::Continue,
            "session-1",
            "task-1",
            Some("coding"),
            Some("verify"),
            Some(3),
        );
        assert_eq!(event.schema, HARNESS_EVENT_SCHEMA_V1);
        let HarnessEventPayload::Retry { ref data } = event.payload else {
            panic!("expected Retry payload");
        };
        assert_eq!(data.session_id, "session-1");
        assert_eq!(data.task_id, "task-1");
        assert_eq!(data.workflow.as_deref(), Some("coding"));
        assert_eq!(data.phase.as_deref(), Some("verify"));
        assert_eq!(data.attempt, Some(3));
        assert_eq!(
            data.extra.get("variant").and_then(|v| v.as_str()),
            Some("rate_limited"),
        );
        assert_eq!(
            data.extra.get("decision").and_then(|v| v.as_str()),
            Some("continue"),
        );
        event.validate().expect("event should validate");
    }

    #[test]
    fn decisions_have_stable_snake_case_labels() {
        // These strings appear as Prometheus labels and in structured events;
        // changing them is a breaking change for dashboards and integrations.
        assert_eq!(LoopDecision::Continue.as_str(), "continue");
        assert_eq!(LoopDecision::RotateAndRetry.as_str(), "rotate_and_retry");
        assert_eq!(LoopDecision::CompactAndRetry.as_str(), "compact_and_retry");
        assert_eq!(LoopDecision::Escalate.as_str(), "escalate");
        assert_eq!(LoopDecision::Exhausted.as_str(), "exhausted");
        assert_eq!(LoopDecision::Grace.as_str(), "grace");
    }

    #[test]
    fn every_harness_variant_has_a_bucket() {
        // If someone adds a HarnessError variant without adding a counter to
        // LoopRetryState, this test catches it at compile time (the match is
        // exhaustive) and at runtime (each variant must bump exactly one
        // counter). The match arms live in `bump_counter`; this test just
        // exercises them so the exhaustiveness check happens under `cargo test`.
        let samples = [
            rate_limit(),
            context_overflow(),
            auth_error(),
            HarnessError::InvalidRequest {
                detail: "x".into(),
                message: "x".into(),
            },
            HarnessError::ContentFiltered {
                message: "x".into(),
            },
            HarnessError::ProviderUnavailable {
                status: Some(503),
                message: "x".into(),
            },
            HarnessError::Network {
                message: "x".into(),
            },
            HarnessError::Timeout {
                message: "x".into(),
            },
            tool_error(),
            HarnessError::PluginSpawn {
                plugin_name: "p".into(),
                message: "x".into(),
            },
            HarnessError::PluginTimeout {
                plugin_name: "p".into(),
                timeout_secs: 5,
                message: "x".into(),
            },
            HarnessError::PluginProtocol {
                plugin_name: "p".into(),
                message: "x".into(),
            },
            HarnessError::DelegateDepthExceeded {
                depth: 3,
                limit: 2,
                message: "x".into(),
            },
            HarnessError::Internal {
                message: "x".into(),
            },
        ];
        let mut state = LoopRetryState::new();
        for err in samples {
            let _ = state.observe(&err);
        }
    }
}
