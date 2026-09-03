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
/// Quota errors are operator-action — auto-retry will keep failing until
/// the operator tops up. Cap at 1 like Authentication.
const DEFAULT_QUOTA_LIMIT: u32 = 1;

/// `#[serde(default = "...")]` helper for `LoopRetryLimits::quota`. Lets
/// legacy retry-state JSON (pre-quota field) deserialize cleanly with the
/// canonical default instead of `0`, which would disable the bucket.
fn default_quota_limit() -> u32 {
    DEFAULT_QUOTA_LIMIT
}
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
    /// Added in codex round-3 (Quota variant). `serde(default)` keeps
    /// legacy retry-state sidecar JSON (pre-quota) deserializable.
    #[serde(default = "default_quota_limit")]
    pub quota: u32,
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
            quota: DEFAULT_QUOTA_LIMIT,
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
///
/// Fields are `pub` for direct reads and test/guard-path mutations, but the
/// delta-merge (`saturating_add_turn_delta`) relies on every bucket being
/// append-only: only ever increment, never decrement or reset (see its doc
/// comment).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRetryCounters {
    pub rate_limited: u32,
    pub context_overflow: u32,
    pub authentication: u32,
    /// Added in codex round-3 (Quota variant). `serde(default)` keeps
    /// legacy retry-state sidecar JSON (pre-quota) deserializable; a
    /// missing field deserializes to `0`.
    #[serde(default)]
    pub quota: u32,
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

impl LoopRetryCounters {
    /// Add another turn's per-bucket increments (`turn` relative to the
    /// `base` it loaded from) onto `self` (#1655). Buckets only ever
    /// increment, so `saturating_sub` yields the turn's own increments and
    /// two overlapping turns both land in the merged state.
    ///
    /// Monotonicity is a load-bearing convention here (#2221): if a future
    /// change ever decrements a bucket, `saturating_sub` clamps the negative
    /// delta to zero and the decrement is silently dropped from the merge.
    /// Bucket mutations must therefore only ever increment — all built-in
    /// mutations go through `observe*` / `observe_shell_spiral`, which do.
    fn saturating_add_turn_delta(&mut self, base: &Self, turn: &Self) {
        self.rate_limited = self
            .rate_limited
            .saturating_add(turn.rate_limited.saturating_sub(base.rate_limited));
        self.context_overflow = self
            .context_overflow
            .saturating_add(turn.context_overflow.saturating_sub(base.context_overflow));
        self.authentication = self
            .authentication
            .saturating_add(turn.authentication.saturating_sub(base.authentication));
        self.quota = self
            .quota
            .saturating_add(turn.quota.saturating_sub(base.quota));
        self.invalid_request = self
            .invalid_request
            .saturating_add(turn.invalid_request.saturating_sub(base.invalid_request));
        self.content_filtered = self
            .content_filtered
            .saturating_add(turn.content_filtered.saturating_sub(base.content_filtered));
        self.provider_unavailable = self.provider_unavailable.saturating_add(
            turn.provider_unavailable
                .saturating_sub(base.provider_unavailable),
        );
        self.network = self
            .network
            .saturating_add(turn.network.saturating_sub(base.network));
        self.timeout = self
            .timeout
            .saturating_add(turn.timeout.saturating_sub(base.timeout));
        self.tool_execution = self
            .tool_execution
            .saturating_add(turn.tool_execution.saturating_sub(base.tool_execution));
        self.plugin_spawn = self
            .plugin_spawn
            .saturating_add(turn.plugin_spawn.saturating_sub(base.plugin_spawn));
        self.plugin_timeout = self
            .plugin_timeout
            .saturating_add(turn.plugin_timeout.saturating_sub(base.plugin_timeout));
        self.plugin_protocol = self
            .plugin_protocol
            .saturating_add(turn.plugin_protocol.saturating_sub(base.plugin_protocol));
        self.delegate_depth_exceeded = self.delegate_depth_exceeded.saturating_add(
            turn.delegate_depth_exceeded
                .saturating_sub(base.delegate_depth_exceeded),
        );
        self.internal = self
            .internal
            .saturating_add(turn.internal.saturating_sub(base.internal));
        self.shell_spiral = self
            .shell_spiral
            .saturating_add(turn.shell_spiral.saturating_sub(base.shell_spiral));
    }
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
    /// [`LoopDecision::Grace`] at most once for this retry state, and only
    /// when there has been at least one productive tool call before the first
    /// budget hit; otherwise returns [`LoopDecision::Escalate`].
    ///
    /// The single grace call is deliberately global to the loop, not one per
    /// productive tool call. Otherwise an agent that keeps making productive
    /// reads after `max_iterations` can run indefinitely.
    pub fn observe_budget_exhaustion(&mut self) -> LoopDecision {
        let decision =
            if self.grace_calls_fired == 0 && self.productive_tool_calls_since_last_grace >= 1 {
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

    /// Merge the delta one turn applied on top of `base` (the state that
    /// turn loaded) into `self`, which a concurrent turn may have advanced
    /// since the load (#1655). Bucket counters and `grace_calls_fired` are
    /// monotonic, so the turn's increments add onto whatever `self` holds
    /// now — a later write-back can no longer roll a bucket back below a
    /// count an earlier turn already reached.
    /// `productive_tool_calls_since_last_grace` is NOT monotonic (a grace
    /// call resets it to zero), so its delta is applied with signed
    /// saturation. `limits` is static configuration, taken from `turn` —
    /// identical to the legacy whole-state write-back in every real flow.
    ///
    /// When `self == base` (no concurrent writer) the result is exactly
    /// `turn`, keeping single-agent behaviour byte-identical.
    pub fn merge_turn_delta(&mut self, base: &Self, turn: &Self) {
        self.counters
            .saturating_add_turn_delta(&base.counters, &turn.counters);
        self.grace_calls_fired = self.grace_calls_fired.saturating_add(
            turn.grace_calls_fired
                .saturating_sub(base.grace_calls_fired),
        );
        let productive_delta = i64::from(turn.productive_tool_calls_since_last_grace)
            - i64::from(base.productive_tool_calls_since_last_grace);
        self.productive_tool_calls_since_last_grace =
            (i64::from(self.productive_tool_calls_since_last_grace) + productive_delta)
                .clamp(0, i64::from(u32::MAX)) as u32;
        self.limits = turn.limits;
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
            HarnessError::Quota { .. } => (&mut self.counters.quota, self.limits.quota),
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

    /// #27b — integration: quota exhaustion routes to `RotateAndRetry`
    /// (the loop advances the provider chain to the fallback slot and
    /// retries), while a TRUE 401 escalates (red line — a wrong key fails
    /// on every lane). Pinned against the live 2026-08-26/27 k3 quota burn.
    #[test]
    fn quota_rotates_lane_but_auth_escalates() {
        let quota = HarnessError::Quota {
            message: "402 quota will reset when the current 7-day window ends".into(),
        };
        let mut state = LoopRetryState::default();
        let decision = state.observe(&quota);
        assert!(
            matches!(decision, LoopDecision::RotateAndRetry),
            "quota must rotate to the fallback lane and retry (#27b), got {decision:?}"
        );

        let auth = HarnessError::Authentication {
            message: "401 invalid api key".into(),
        };
        let mut state = LoopRetryState::default();
        let decision = state.observe(&auth);
        assert!(
            matches!(decision, LoopDecision::Escalate),
            "a true 401 must escalate (FailFast red line), got {decision:?}"
        );
    }

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
    fn grace_call_is_single_use_even_after_fresh_productive_tool_call() {
        let mut state = LoopRetryState::new();
        state.record_productive_tool_call();
        assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Grace);
        state.record_productive_tool_call();
        assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Escalate);
        assert_eq!(state.grace_calls_fired, 1);
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
    fn should_merge_every_field_when_turn_delta_applied() {
        // Field-coverage guard (#2221): the literals below name EVERY field
        // of `LoopRetryState`, `LoopRetryCounters`, and `LoopRetryLimits`
        // with no `..Default::default()` spread, so adding a field to any of
        // the three structs fails THIS test's compilation — forcing the
        // author to extend `merge_turn_delta` (and
        // `LoopRetryCounters::saturating_add_turn_delta`) in the same commit.
        // `turn` also gives each field a DISTINCT delta over `base`
        // (2 + field offset), so the final whole-struct equality catches
        // both a forgotten merge line (the field keeps self's value) and a
        // mis-wired one (a copy-pasted line reading the wrong base/turn
        // field lands the wrong delta).
        let uniform_counters = |v: u32| LoopRetryCounters {
            rate_limited: v,
            context_overflow: v,
            authentication: v,
            quota: v,
            invalid_request: v,
            content_filtered: v,
            provider_unavailable: v,
            network: v,
            timeout: v,
            tool_execution: v,
            plugin_spawn: v,
            plugin_timeout: v,
            plugin_protocol: v,
            delegate_depth_exceeded: v,
            internal: v,
            shell_spiral: v,
        };
        let offset_counters = |v: u32| LoopRetryCounters {
            rate_limited: v,
            context_overflow: v + 1,
            authentication: v + 2,
            quota: v + 3,
            invalid_request: v + 4,
            content_filtered: v + 5,
            provider_unavailable: v + 6,
            network: v + 7,
            timeout: v + 8,
            tool_execution: v + 9,
            plugin_spawn: v + 10,
            plugin_timeout: v + 11,
            plugin_protocol: v + 12,
            delegate_depth_exceeded: v + 13,
            internal: v + 14,
            shell_spiral: v + 15,
        };
        let uniform_limits = |v: u32| LoopRetryLimits {
            rate_limited: v,
            context_overflow: v,
            authentication: v,
            quota: v,
            invalid_request: v,
            content_filtered: v,
            provider_unavailable: v,
            network: v,
            timeout: v,
            tool_execution: v,
            plugin_spawn: v,
            plugin_timeout: v,
            plugin_protocol: v,
            delegate_depth_exceeded: v,
            internal: v,
            shell_spiral: v,
        };

        let base = LoopRetryState {
            counters: uniform_counters(1),
            limits: uniform_limits(100),
            productive_tool_calls_since_last_grace: 1,
            grace_calls_fired: 1,
        };
        let turn = LoopRetryState {
            counters: offset_counters(3),
            limits: uniform_limits(200),
            productive_tool_calls_since_last_grace: 4,
            grace_calls_fired: 2,
        };
        let mut merged = LoopRetryState {
            counters: uniform_counters(10),
            limits: uniform_limits(300),
            productive_tool_calls_since_last_grace: 5,
            grace_calls_fired: 5,
        };
        merged.merge_turn_delta(&base, &turn);

        let expected = LoopRetryState {
            // 10 + ((3 + i) - 1) = 12 + i: each bucket gains exactly its OWN
            // turn delta, so a cross-field mis-wire flips the value.
            counters: offset_counters(12),
            // Static configuration, taken wholesale from `turn`.
            limits: uniform_limits(200),
            // 5 + (4 - 1): signed delta for the non-monotonic field.
            productive_tool_calls_since_last_grace: 8,
            // 5 + (2 - 1): monotonic saturating delta.
            grace_calls_fired: 6,
        };
        assert_eq!(merged, expected);
    }

    #[test]
    fn legacy_retry_state_json_without_quota_field_deserializes() {
        // Codex round-6: `LoopRetryLimits` and `LoopRetryCounters` gained a
        // `quota` field in round-3. Without `#[serde(default)]`, retry-state
        // sidecar JSON written by pre-round-3 builds would fail to
        // deserialize, and `load_retry_state` would silently reset every
        // counter to zero. This test pins the backward-compat behavior.
        let legacy_json = r#"{
            "counters": {
                "rate_limited": 3,
                "context_overflow": 1,
                "authentication": 0,
                "invalid_request": 0,
                "content_filtered": 0,
                "provider_unavailable": 2,
                "network": 1,
                "timeout": 0,
                "tool_execution": 0,
                "plugin_spawn": 0,
                "plugin_timeout": 0,
                "plugin_protocol": 0,
                "delegate_depth_exceeded": 0,
                "internal": 0,
                "shell_spiral": 0
            },
            "limits": {
                "rate_limited": 5,
                "context_overflow": 2,
                "authentication": 1,
                "invalid_request": 2,
                "content_filtered": 1,
                "provider_unavailable": 4,
                "network": 4,
                "timeout": 3,
                "tool_execution": 5,
                "plugin_spawn": 2,
                "plugin_timeout": 3,
                "plugin_protocol": 2,
                "delegate_depth_exceeded": 1,
                "internal": 1,
                "shell_spiral": 1
            },
            "productive_tool_calls_since_last_grace": 4,
            "grace_calls_fired": 0
        }"#;

        let state: LoopRetryState =
            serde_json::from_str(legacy_json).expect("legacy JSON should deserialize");

        // Existing counters preserved.
        assert_eq!(state.counters.rate_limited, 3);
        assert_eq!(state.counters.provider_unavailable, 2);
        // Missing quota counter defaults to 0 (a clean Default::default()).
        assert_eq!(state.counters.quota, 0);
        // Missing quota limit defaults to the canonical DEFAULT_QUOTA_LIMIT
        // (1) — not 0, which would disable the bucket entirely.
        assert_eq!(state.limits.quota, DEFAULT_QUOTA_LIMIT);
        // Productive history preserved so grace-call gating survives.
        assert_eq!(state.productive_tool_calls_since_last_grace, 4);
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

    #[test]
    fn merge_turn_delta_adds_increments_onto_concurrently_advanced_state() {
        // Two turns loaded the same base; each bumped different buckets.
        // Merging turn B's delta onto the state turn A already wrote must
        // preserve BOTH turns' increments (#1655).
        let base = LoopRetryState::default();
        let mut turn = base.clone();
        turn.counters.rate_limited = 1;
        turn.counters.network = 3;

        let mut shared = LoopRetryState {
            counters: LoopRetryCounters {
                rate_limited: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        shared.merge_turn_delta(&base, &turn);
        assert_eq!(shared.counters.rate_limited, 3);
        assert_eq!(shared.counters.network, 3);
    }

    #[test]
    fn merge_turn_delta_applies_grace_reset_as_negative_delta() {
        // `productive_tool_calls_since_last_grace` resets to zero when a
        // grace call fires — the one non-monotonic field. Its delta must
        // subtract, not clamp at the base.
        let base = LoopRetryState {
            productive_tool_calls_since_last_grace: 3,
            ..Default::default()
        };
        let mut turn = base.clone();
        turn.observe_budget_exhaustion(); // grace: resets the counter, fires once

        let mut shared = base.clone();
        shared.merge_turn_delta(&base, &turn);
        assert_eq!(shared.productive_tool_calls_since_last_grace, 0);
        assert_eq!(shared.grace_calls_fired, 1);
    }

    #[test]
    fn merge_turn_delta_is_identity_without_concurrent_writer() {
        // Single-agent regression (#1655): when the shared state still
        // equals what the turn loaded, the merge reproduces the turn's
        // state exactly — byte-identical to the legacy write-back.
        let base = LoopRetryState {
            counters: LoopRetryCounters {
                timeout: 2,
                ..Default::default()
            },
            productive_tool_calls_since_last_grace: 4,
            ..Default::default()
        };
        let mut turn = base.clone();
        turn.counters.timeout = 3;
        turn.observe_budget_exhaustion();

        let mut shared = base.clone();
        shared.merge_turn_delta(&base, &turn);
        assert_eq!(shared, turn);
    }
}
