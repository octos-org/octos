//! Typed turn state for loop execution.

use std::time::Instant;

use octos_core::TokenUsage;

use super::activity::LoopActivityState;
use super::budget::BudgetStop;
use super::{Agent, TokenTracker};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopBudgetStopKind {
    Shutdown,
    MaxIterations,
    MaxTokens,
    ActivityTimeout,
    IdleProgressTimeout,
}

impl From<&BudgetStop> for LoopBudgetStopKind {
    fn from(value: &BudgetStop) -> Self {
        match value {
            BudgetStop::Shutdown => Self::Shutdown,
            BudgetStop::MaxIterations { .. } => Self::MaxIterations,
            BudgetStop::MaxTokens { .. } => Self::MaxTokens,
            BudgetStop::ActivityTimeout { .. } => Self::ActivityTimeout,
            BudgetStop::IdleProgressTimeout { .. } => Self::IdleProgressTimeout,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoopTerminalReason {
    Budget {
        kind: LoopBudgetStopKind,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoopRetryReason {
    EmptyResponse { attempt: u32, reason: String },
    StreamError { attempt: u32, error: String },
    ProviderFailover { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoopRepairReason {
    ContextTrimmed,
    SystemMessagesNormalized,
    MessageOrderRepaired,
    ToolPairsRepaired,
    MissingToolResultsSynthesized,
    OldToolResultsTruncated,
    ToolCallIdsNormalized,
}

#[derive(Debug, Clone)]
pub(crate) struct LoopTurnState {
    started_at: Instant,
    iteration: u32,
    total_usage: TokenUsage,
    /// Estimated spend for THIS turn: the sum of per-response costs,
    /// each priced at the model that actually produced the response.
    /// The old emission path instead re-priced the whole turn's token
    /// totals at whichever model answered LAST, silently re-pricing
    /// earlier iterations whenever failover/routing switched models
    /// mid-turn.
    turn_spend_usd: f64,
    /// Whether any recorded usage carried a price. Distinguishes "spend
    /// is genuinely $0 so far" from "no model in this turn had catalog
    /// pricing" — emission hides the cost line in the latter case.
    priced_usage: bool,
    retry_reasons: Vec<LoopRetryReason>,
    repair_reasons: Vec<LoopRepairReason>,
    terminal_reason: Option<LoopTerminalReason>,
}

impl LoopTurnState {
    pub(crate) fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            iteration: 0,
            total_usage: TokenUsage::default(),
            turn_spend_usd: 0.0,
            priced_usage: false,
            retry_reasons: Vec::new(),
            repair_reasons: Vec::new(),
            terminal_reason: None,
        }
    }

    pub(crate) fn iteration(&self) -> u32 {
        self.iteration
    }

    pub(crate) fn advance_iteration(&mut self) -> u32 {
        self.iteration += 1;
        self.iteration
    }

    pub(crate) fn total_usage(&self) -> &TokenUsage {
        &self.total_usage
    }

    /// Sum of per-response estimated costs recorded this turn, each
    /// priced at the model that produced it.
    pub(crate) fn spend_usd(&self) -> f64 {
        self.turn_spend_usd
    }

    /// True once at least one recorded response carried a price.
    pub(crate) fn has_priced_usage(&self) -> bool {
        self.priced_usage
    }

    /// `spend_usd` gated on any usage having been priced — the shape
    /// `ConversationResponse::estimated_spend_usd` carries.
    pub(crate) fn priced_spend(&self) -> Option<f64> {
        self.priced_usage.then_some(self.turn_spend_usd)
    }

    #[cfg(test)]
    pub(crate) fn retry_reasons(&self) -> &[LoopRetryReason] {
        &self.retry_reasons
    }

    #[cfg(test)]
    pub(crate) fn repair_reasons(&self) -> &[LoopRepairReason] {
        &self.repair_reasons
    }

    /// Record one response's usage. `estimated_cost_usd` is that usage
    /// priced at the model which produced it (`None` when the model has
    /// no catalog pricing) — the explicit parameter forces every call
    /// site to decide the attribution instead of a later pass re-pricing
    /// the turn total at the wrong model.
    pub(crate) fn record_usage(
        &mut self,
        usage: &TokenUsage,
        tracker: Option<&TokenTracker>,
        estimated_cost_usd: Option<f64>,
    ) {
        self.total_usage.input_tokens += usage.input_tokens;
        self.total_usage.output_tokens += usage.output_tokens;
        // Reasoning is a reported component of output, not extra billable
        // volume. Retain it for diagnostics without adding it to output,
        // pricing, active-token thresholds, or the existing tracker counters.
        self.total_usage.reasoning_tokens += usage.reasoning_tokens;
        // Cache traffic is real processed prompt volume — Anthropic reports
        // it OUTSIDE input_tokens (disjoint accounting) — so accumulate it
        // too, keeping the turn totals and the token-budget gate at their
        // pre-caching meaning (everything the provider processed).
        self.total_usage.cache_read_tokens += usage.cache_read_tokens;
        self.total_usage.cache_write_tokens += usage.cache_write_tokens;
        if let Some(cost) = estimated_cost_usd {
            self.turn_spend_usd += cost;
            self.priced_usage = true;
        }
        if let Some(tracker) = tracker {
            tracker.input_tokens.store(
                self.total_usage.input_tokens,
                std::sync::atomic::Ordering::Relaxed,
            );
            tracker.output_tokens.store(
                self.total_usage.output_tokens,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    /// Adapt provider usage to the core's counter type in one place. The
    /// provider-only semantic checkpoint report is not an additive counter.
    pub(crate) fn record_llm_usage(
        &mut self,
        usage: &octos_llm::TokenUsage,
        tracker: Option<&TokenTracker>,
        estimated_cost_usd: Option<f64>,
    ) {
        self.record_usage(
            &TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
            },
            tracker,
            estimated_cost_usd,
        );
    }

    pub(crate) fn record_retry(&mut self, reason: LoopRetryReason) {
        self.retry_reasons.push(reason);
    }

    pub(crate) fn record_repair(&mut self, reason: LoopRepairReason) {
        self.repair_reasons.push(reason);
    }

    pub(crate) fn check_budget(
        &self,
        agent: &Agent,
        activity: &LoopActivityState,
    ) -> Option<BudgetStop> {
        agent.check_budget(self.iteration, self.started_at, &self.total_usage, activity)
    }

    pub(crate) fn record_budget_stop(&mut self, stop: &BudgetStop) {
        self.terminal_reason = Some(LoopTerminalReason::Budget {
            kind: LoopBudgetStopKind::from(stop),
            message: stop.message(),
        });
    }

    // NOTE: `attach_partial_usage` / `PartialTurnUsage` (below) is how an
    // error EXIT of the loop carries this `total_usage` out. The happy path
    // returns it via `ConversationResponse.token_usage` / `TaskResult`, but a
    // bare `return Err(report)` on an error/interrupt would otherwise drop it
    // (`eyre::Report` has no usage channel), so an errored/rate-limited peer
    // or goal turn charges 0 tokens (issue #1969).

    // NOTE: `new_messages` / `new_output_start` were removed in NEW-16
    // (fix/persist-from-append-only-turn-log-not-mutated-buffer).
    //
    // They sliced the LLM prompt buffer at `1 + history_len`, which was
    // unstable because that buffer is mutated during the loop by
    // `prepare_conversation_messages` (which calls `repair_message_order`)
    // and by the AppUI bridge in `ui_protocol.rs`. After mutation, OLD
    // rows from prior turns could end up past the stale boundary and be
    // returned as "new", which caused the cross-turn drag-forward
    // re-persistence bug (mini3 Yuan-dynasty content showing up 7x in
    // a single session, 2026-05-23 soak).
    //
    // The replacement is the append-only `turn_output_log` built in
    // `process_message_inner` (see `loop_runner.rs`). It is never read
    // back from — only pushed to — so no downstream mutation pass can
    // shift OLD rows into it.
}

/// Downcastable carrier for a turn's accumulated token usage, attached to the
/// eyre error when the agent loop bails on an error/interrupt (issue #1969).
///
/// The happy path surfaces per-turn usage via `ConversationResponse.token_usage`
/// / `TaskResult.token_usage`. Every error exit, though, was a bare
/// `return Err(report)` and `eyre::Report` has no usage channel — so the
/// turn's real spend was dropped and a downstream peer/goal accountant charged
/// 0 tokens for a turn that had already burned real tokens before failing.
///
/// Attach via [`attach_partial_usage`]. It uses eyre's `wrap_err`, which is the
/// ONE attachment mechanism that keeps the wrapped report downcastable to BOTH
/// this carrier AND the original error (e.g. `LlmError`). That dual-downcast is
/// load-bearing: `HarnessError::classify_report` and the CLI's
/// `classify_runtime_error_message` reach the underlying `LlmError` (via
/// `Report::downcast_ref` and via `chain()` + std downcast respectively) to
/// drive retry/breaker classification, and must keep working through the
/// wrapper.
#[derive(Debug, Clone)]
pub struct PartialTurnUsage {
    /// The turn's accumulated usage at the moment it bailed.
    pub total: TokenUsage,
    /// The wrapped error's outer `Display`, captured at attach time. Because
    /// `wrap_err` makes THIS carrier the outermost layer, reproducing the
    /// original message here keeps `report.to_string()` (and the
    /// `classify_runtime_error_message` fallback) reporting the real error
    /// instead of a usage note.
    display: String,
}

impl std::fmt::Display for PartialTurnUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display)
    }
}

/// Attach a turn's accumulated `total` usage to an outgoing error WITHOUT
/// hiding the original error (issue #1969).
///
/// `wrap_err` layers a [`PartialTurnUsage`] context over `err`; the inner
/// error stays reachable through both eyre's `Report::downcast_ref` and
/// `chain()` + std downcast, so classification/retry logic is unchanged. The
/// captured display string keeps the wrapper's own `Display` equal to the
/// original error's outer message.
pub(crate) fn attach_partial_usage(err: eyre::Report, total: TokenUsage) -> eyre::Report {
    let display = err.to_string();
    err.wrap_err(PartialTurnUsage { total, display })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_carry_usage_and_preserve_inner_error_when_attached() {
        use octos_llm::LlmError;
        // A Report built FROM an LlmError, mirroring the loop's LLM bail.
        let report: eyre::Report = LlmError::rate_limited(Some(2)).into();
        let original = report.to_string();
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            ..Default::default()
        };

        let wrapped = attach_partial_usage(report, usage);

        // Carrier is extractable with the accumulated totals.
        let carried = wrapped
            .downcast_ref::<PartialTurnUsage>()
            .expect("carrier attached");
        assert_eq!(carried.total.input_tokens, 1_000);
        assert_eq!(carried.total.output_tokens, 500);
        // Inner LlmError still reachable (classification/retry logic).
        assert!(wrapped.downcast_ref::<LlmError>().is_some());
        assert!(
            wrapped
                .chain()
                .any(|c| c.downcast_ref::<LlmError>().is_some())
        );
        // Outer Display unchanged (no usage note buried the real message).
        assert_eq!(wrapped.to_string(), original);
    }

    #[test]
    fn records_explicit_budget_terminal_reason() {
        let mut state = LoopTurnState::new(Instant::now());
        assert_eq!(state.iteration(), 0);

        state.advance_iteration();
        state.record_budget_stop(&BudgetStop::MaxTokens {
            used: 120,
            limit: 100,
        });

        assert_eq!(
            state.terminal_reason.clone(),
            Some(LoopTerminalReason::Budget {
                kind: LoopBudgetStopKind::MaxTokens,
                message: "Token budget exceeded (120 of 100).".to_string(),
            })
        );
    }

    #[test]
    fn should_sum_per_response_costs_when_usage_recorded_across_models() {
        let mut state = LoopTurnState::new(Instant::now());
        assert!(!state.has_priced_usage());
        assert!((state.spend_usd() - 0.0).abs() < f64::EPSILON);

        // Two responses priced at DIFFERENT models' rates plus one from
        // an unpriced model: tokens all count, spend sums only the known
        // costs — no re-pricing of earlier responses at the last model.
        let usage = |input_tokens, output_tokens| TokenUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        };
        state.record_usage(&usage(1_000, 200), None, Some(0.015));
        state.record_usage(&usage(2_000, 400), None, Some(0.002));
        state.record_usage(&usage(500, 100), None, None);

        assert_eq!(state.total_usage().input_tokens, 3_500);
        assert_eq!(state.total_usage().output_tokens, 700);
        assert!(state.has_priced_usage());
        assert!((state.spend_usd() - 0.017).abs() < 1e-9);
    }

    #[test]
    fn should_accumulate_cache_tokens_when_usage_recorded() {
        // Anthropic reports cached prefix tokens OUTSIDE input_tokens; the
        // turn total must carry them so the token budget and downstream
        // ledgers see the full processed volume.
        let mut state = LoopTurnState::new(Instant::now());
        state.record_usage(
            &TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                reasoning_tokens: 30,
                cache_read_tokens: 4_000,
                cache_write_tokens: 850,
            },
            None,
            None,
        );
        assert_eq!(state.total_usage().input_tokens, 100);
        assert_eq!(state.total_usage().output_tokens, 50);
        assert_eq!(state.total_usage().reasoning_tokens, 30);
        assert_eq!(state.total_usage().cache_read_tokens, 4_000);
        assert_eq!(state.total_usage().cache_write_tokens, 850);
    }

    #[test]
    fn records_retry_and_repair_history() {
        let mut state = LoopTurnState::new(Instant::now());

        state.record_retry(LoopRetryReason::EmptyResponse {
            attempt: 1,
            reason: "empty response".to_string(),
        });
        state.record_retry(LoopRetryReason::ProviderFailover {
            reason: "streaming retries exhausted".to_string(),
        });
        state.record_repair(LoopRepairReason::ContextTrimmed);
        state.record_repair(LoopRepairReason::ToolCallIdsNormalized);

        assert_eq!(
            state.retry_reasons(),
            &[
                LoopRetryReason::EmptyResponse {
                    attempt: 1,
                    reason: "empty response".to_string(),
                },
                LoopRetryReason::ProviderFailover {
                    reason: "streaming retries exhausted".to_string(),
                },
            ]
        );
        assert_eq!(
            state.repair_reasons(),
            &[
                LoopRepairReason::ContextTrimmed,
                LoopRepairReason::ToolCallIdsNormalized,
            ]
        );
    }
}
