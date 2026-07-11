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
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
        tracker: Option<&TokenTracker>,
        estimated_cost_usd: Option<f64>,
    ) {
        self.total_usage.input_tokens += input_tokens;
        self.total_usage.output_tokens += output_tokens;
        // Cache traffic is real processed prompt volume — Anthropic reports
        // it OUTSIDE input_tokens (disjoint accounting) — so accumulate it
        // too, keeping the turn totals and the token-budget gate at their
        // pre-caching meaning (everything the provider processed).
        self.total_usage.cache_read_tokens += cache_read_tokens;
        self.total_usage.cache_write_tokens += cache_write_tokens;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        state.record_usage(1_000, 200, 0, 0, None, Some(0.015));
        state.record_usage(2_000, 400, 0, 0, None, Some(0.002));
        state.record_usage(500, 100, 0, 0, None, None);

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
        state.record_usage(100, 50, 4_000, 850, None, None);
        assert_eq!(state.total_usage().input_tokens, 100);
        assert_eq!(state.total_usage().output_tokens, 50);
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
