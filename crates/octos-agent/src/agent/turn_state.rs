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

    #[cfg(test)]
    pub(crate) fn retry_reasons(&self) -> &[LoopRetryReason] {
        &self.retry_reasons
    }

    #[cfg(test)]
    pub(crate) fn repair_reasons(&self) -> &[LoopRepairReason] {
        &self.repair_reasons
    }

    pub(crate) fn record_usage(
        &mut self,
        input_tokens: u32,
        output_tokens: u32,
        tracker: Option<&TokenTracker>,
    ) {
        self.total_usage.input_tokens += input_tokens;
        self.total_usage.output_tokens += output_tokens;
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
