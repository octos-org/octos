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
    WallClockTimeout,
    IdleProgressTimeout,
}

impl From<&BudgetStop> for LoopBudgetStopKind {
    fn from(value: &BudgetStop) -> Self {
        match value {
            BudgetStop::Shutdown => Self::Shutdown,
            BudgetStop::MaxIterations => Self::MaxIterations,
            BudgetStop::MaxTokens { .. } => Self::MaxTokens,
            BudgetStop::WallClockTimeout { .. } => Self::WallClockTimeout,
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

#[derive(Debug, Clone)]
pub(crate) struct LoopTurnState {
    started_at: Instant,
    iteration: u32,
    total_usage: TokenUsage,
    terminal_reason: Option<LoopTerminalReason>,
}

impl LoopTurnState {
    pub(crate) fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            iteration: 0,
            total_usage: TokenUsage::default(),
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

    pub(crate) fn new_output_start(history_len: usize, messages_len: usize) -> usize {
        (1 + history_len).min(messages_len)
    }

    pub(crate) fn new_messages(
        messages: &[octos_core::Message],
        history_len: usize,
    ) -> Vec<octos_core::Message> {
        let new_start = Self::new_output_start(history_len, messages.len());
        messages[new_start..].to_vec()
    }
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
}
