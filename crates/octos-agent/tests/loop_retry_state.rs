//! Acceptance tests for M6.2 loop retry-bucket state machine (issue #489).
//!
//! These tests pin the public contract required by the milestone:
//!   - [`LoopRetryState::observe`] escalates after the invalid-request
//!     bucket is exhausted.
//!   - `ContextOverflow` observations return [`LoopDecision::CompactAndRetry`].
//!   - Budget exhaustion + productive history triggers a single
//!     [`LoopDecision::Grace`]; without productive history it escalates.
//!   - The state round-trips through serde so compaction can persist it.
//!   - The new dispatch path preserves the byte-for-byte behavior of the
//!     pre-M6.2 `recover_shell_retry` helper (shell-spiral tests still
//!     detect + return the same recovery content).

use octos_agent::harness_errors::HarnessError;
use octos_agent::{LoopDecision, LoopRetryLimits, LoopRetryState};

// ─────────────────────────────────────────────────────────────────────────
// Bucket exhaustion: InvalidRequest
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_escalate_after_invalid_tool_call_limit() {
    let mut state = LoopRetryState::with_limits(LoopRetryLimits {
        invalid_request: 2,
        ..Default::default()
    });

    // `InvalidRequest` is mapped to `FailFast` in the M6.1 taxonomy, so the
    // per-observation decision is always `Escalate` — but the *bucket*
    // exhaustion is still tracked and returns `Exhausted` once the counter
    // exceeds the limit. Callers treat `Exhausted` as a hard stop (invariant
    // #2 from #489) and never silently loop.
    let err = HarnessError::InvalidRequest {
        detail: "bad schema".into(),
        message: "400".into(),
    };
    assert_eq!(state.observe(&err), LoopDecision::Escalate);
    assert_eq!(state.observe(&err), LoopDecision::Escalate);
    assert_eq!(state.observe(&err), LoopDecision::Exhausted);
}

// ─────────────────────────────────────────────────────────────────────────
// Context overflow → CompactAndRetry
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_compact_on_context_overflow_decision() {
    let mut state = LoopRetryState::with_limits(LoopRetryLimits {
        context_overflow: 3,
        ..Default::default()
    });
    let err = HarnessError::ContextOverflow {
        limit: Some(200_000),
        used: Some(201_000),
        message: "context exceeded".into(),
    };

    // First three observations map to CompactAndRetry (compaction is the only
    // viable recovery for this variant). The fourth exhausts the bucket.
    assert_eq!(state.observe(&err), LoopDecision::CompactAndRetry);
    assert_eq!(state.observe(&err), LoopDecision::CompactAndRetry);
    assert_eq!(state.observe(&err), LoopDecision::CompactAndRetry);
    assert_eq!(state.observe(&err), LoopDecision::Exhausted);
}

// ─────────────────────────────────────────────────────────────────────────
// Grace-call gating at budget exhaustion
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_fire_grace_call_at_budget_exhaustion_with_productive_history() {
    let mut state = LoopRetryState::new();
    state.record_productive_tool_call();

    // Productive history exists, so the first budget-exhaustion observation
    // returns Grace. `grace_calls_fired` is bumped so operators can observe
    // the event, and the productive counter is consumed.
    assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Grace);
    assert_eq!(state.grace_calls_fired, 1);
    assert_eq!(state.productive_tool_calls_since_last_grace, 0);
}

#[test]
fn should_not_fire_grace_call_without_productive_history() {
    let mut state = LoopRetryState::new();

    // No productive tool calls recorded → budget exhaustion is an immediate
    // Escalate. The loop is stalled and must not be handed a free iteration.
    assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Escalate);
    assert_eq!(state.grace_calls_fired, 0);
}

#[test]
fn should_require_fresh_productive_history_between_grace_calls() {
    // A second budget-exhaustion after Grace was fired must escalate unless a
    // fresh productive tool call is recorded. This prevents loops that
    // merely accumulate productive calls before budget from chaining
    // grace calls indefinitely.
    let mut state = LoopRetryState::new();
    state.record_productive_tool_call();
    state.record_productive_tool_call();
    assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Grace);
    // Without recording more productive calls, the second exhaustion
    // escalates even though the loop once was productive.
    assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Escalate);

    state.record_productive_tool_call();
    assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Grace);
    assert_eq!(state.grace_calls_fired, 2);
}

// ─────────────────────────────────────────────────────────────────────────
// Serde round-trip (state survives compaction)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_serde_round_trip_loop_retry_state() {
    let mut state = LoopRetryState::new();
    state.observe(&HarnessError::RateLimited {
        retry_after_secs: Some(1),
        message: "429".into(),
    });
    state.observe(&HarnessError::ContextOverflow {
        limit: Some(200_000),
        used: Some(201_000),
        message: "context exceeded".into(),
    });
    state.observe(&HarnessError::Network {
        message: "DNS failed".into(),
    });
    state.record_productive_tool_call();
    state.record_productive_tool_call();

    let json = serde_json::to_string(&state).expect("serialize");
    let restored: LoopRetryState = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(state, restored);

    // Post-round-trip the grace-call pathway must still work: productive
    // history should be preserved and the next budget-exhaustion grants Grace.
    let mut restored = restored;
    assert_eq!(
        restored.observe_budget_exhaustion(),
        LoopDecision::Grace,
        "grace call should fire after compaction round-trip"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Shell-spiral dispatch preserves pre-M6.2 behavior
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_preserve_shell_spiral_recovery_behavior() {
    // The shell-spiral bucket is synthetic (not a HarnessError variant), so
    // the observation path uses `observe_shell_spiral`. The first spiral
    // observation returns Escalate — the same stop-and-surface behavior the
    // pre-M6.2 `recover_shell_retry` helper produced — and further spirals
    // exhaust the bucket.
    //
    // The detailed byte-level detector behavior (diff detection, validation
    // detection, retry-limit banner) is covered by the inline unit tests in
    // `loop_runner.rs` (`recover_shell_retry_output_*`). Those tests continue
    // to pass unchanged because the detector itself was not modified; this
    // acceptance test pins the state-machine dispatch layer.
    let mut state = LoopRetryState::with_limits(LoopRetryLimits {
        shell_spiral: 1,
        ..Default::default()
    });
    assert_eq!(state.observe_shell_spiral(), LoopDecision::Escalate);
    assert_eq!(state.observe_shell_spiral(), LoopDecision::Exhausted);
    assert_eq!(state.counters().shell_spiral, 2);
}

// ─────────────────────────────────────────────────────────────────────────
// Decision labels are stable snake_case
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_expose_stable_decision_labels_for_metrics() {
    // These strings appear as Prometheus labels on `octos_loop_retry_total`
    // and in the typed `HarnessEventPayload::Retry` event — changing them is
    // a breaking change for dashboards.
    assert_eq!(LoopDecision::Continue.as_str(), "continue");
    assert_eq!(LoopDecision::RotateAndRetry.as_str(), "rotate_and_retry");
    assert_eq!(LoopDecision::CompactAndRetry.as_str(), "compact_and_retry");
    assert_eq!(LoopDecision::Escalate.as_str(), "escalate");
    assert_eq!(LoopDecision::Exhausted.as_str(), "exhausted");
    assert_eq!(LoopDecision::Grace.as_str(), "grace");
    for label in [
        "continue",
        "rotate_and_retry",
        "compact_and_retry",
        "escalate",
        "exhausted",
        "grace",
    ] {
        assert!(
            label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "decision label '{label}' must be snake_case ASCII",
        );
    }
}
