//! Budget tracking and enforcement for the agent loop.

use std::time::{Duration, Instant};

use octos_core::TokenUsage;
use tracing::{info, warn};

use super::Agent;
use super::activity::{DEFAULT_IDLE_TIMEOUT_SECS, LoopActivityState};
use crate::progress::ProgressEvent;

/// Reason why the agent loop stopped due to budget constraints.
pub(super) enum BudgetStop {
    Shutdown,
    MaxIterations,
    MaxTokens { used: u32, limit: u32 },
    WallClockTimeout { limit: Duration },
    IdleProgressTimeout { limit: Duration },
}

impl BudgetStop {
    pub(super) fn message(&self) -> String {
        match self {
            Self::Shutdown => String::new(),
            Self::MaxIterations => "Reached max iterations.".into(),
            Self::MaxTokens { used, limit } => {
                format!("Token budget exceeded ({used} of {limit}).")
            }
            Self::WallClockTimeout { limit } => {
                format!("Wall-clock timeout ({:.0}s limit).", limit.as_secs_f64())
            }
            Self::IdleProgressTimeout { limit } => {
                format!(
                    "Idle progress timeout ({:.0}s without progress).",
                    limit.as_secs_f64()
                )
            }
        }
    }
}

impl Agent {
    /// Check whether the agent loop should stop due to budget constraints.
    pub(super) fn check_budget(
        &self,
        iteration: u32,
        start: Instant,
        total_usage: &TokenUsage,
        activity: &LoopActivityState,
    ) -> Option<BudgetStop> {
        use std::sync::atomic::Ordering;

        if self.shutdown.load(Ordering::Acquire) {
            return Some(BudgetStop::Shutdown);
        }
        if iteration >= self.config.max_iterations {
            return Some(BudgetStop::MaxIterations);
        }
        if let Some(timeout) = self.config.max_timeout {
            if start.elapsed() > timeout {
                return Some(BudgetStop::WallClockTimeout { limit: timeout });
            }
        }
        let idle_timeout = Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS);
        if activity.has_timed_out(idle_timeout) {
            return Some(BudgetStop::IdleProgressTimeout {
                limit: idle_timeout,
            });
        }
        if let Some(max_tokens) = self.config.max_tokens {
            let used = total_usage.input_tokens + total_usage.output_tokens;
            if used >= max_tokens {
                return Some(BudgetStop::MaxTokens {
                    used,
                    limit: max_tokens,
                });
            }
        }
        None
    }

    /// Log and report a budget stop event (used by `run_task`).
    pub(super) fn report_budget_stop(&self, stop: &BudgetStop, iteration: u32) {
        match stop {
            BudgetStop::Shutdown => {
                info!(iteration, "shutdown signal received");
                self.reporter().report(ProgressEvent::TaskInterrupted {
                    iterations: iteration,
                });
            }
            BudgetStop::MaxIterations => {
                warn!(
                    iteration,
                    max = self.config.max_iterations,
                    "hit max iterations limit"
                );
                self.reporter().report(ProgressEvent::MaxIterationsReached {
                    limit: self.config.max_iterations,
                });
            }
            BudgetStop::MaxTokens { used, limit } => {
                warn!(used, max = limit, "hit token budget limit");
                self.reporter().report(ProgressEvent::TokenBudgetExceeded {
                    used: *used,
                    limit: *limit,
                });
            }
            BudgetStop::WallClockTimeout { limit } => {
                warn!(limit_s = limit.as_secs(), "hit wall-clock timeout");
                self.reporter()
                    .report(ProgressEvent::WallClockTimeoutReached {
                        elapsed: *limit,
                        limit: *limit,
                    });
            }
            BudgetStop::IdleProgressTimeout { limit } => {
                warn!(limit_s = limit.as_secs(), "hit idle progress timeout");
                self.reporter().report(ProgressEvent::TaskInterrupted {
                    iterations: iteration,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::super::{AgentConfig, TokenTracker};

    // ---------- AgentConfig::default ----------

    #[test]
    fn agent_config_default_values() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.max_iterations, 50);
        assert_eq!(cfg.max_tokens, None);
        assert_eq!(cfg.max_timeout, Some(Duration::from_secs(600)));
        assert!(cfg.save_episodes);
        assert_eq!(cfg.tool_timeout_secs, 600);
        assert!(cfg.worker_prompt.is_none());
    }

    // ---------- TokenTracker ----------

    #[test]
    fn token_tracker_new_starts_at_zero() {
        let t = TokenTracker::new();
        assert_eq!(t.input_tokens.load(Ordering::Relaxed), 0);
        assert_eq!(t.output_tokens.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn token_tracker_default_starts_at_zero() {
        let t = TokenTracker::default();
        assert_eq!(t.input_tokens.load(Ordering::Relaxed), 0);
        assert_eq!(t.output_tokens.load(Ordering::Relaxed), 0);
    }

    // ---------- BudgetStop::message ----------

    #[test]
    fn budget_stop_shutdown_message() {
        assert_eq!(BudgetStop::Shutdown.message(), "");
    }

    #[test]
    fn budget_stop_max_iterations_message() {
        assert_eq!(
            BudgetStop::MaxIterations.message(),
            "Reached max iterations."
        );
    }

    #[test]
    fn budget_stop_max_tokens_message() {
        let msg = BudgetStop::MaxTokens {
            used: 1000,
            limit: 500,
        }
        .message();
        assert!(
            msg.contains("token") || msg.contains("Token") || msg.contains("TOKEN"),
            "expected 'token' in: {msg}"
        );
        assert!(msg.contains("1000"), "expected '1000' in: {msg}");
        assert!(msg.contains("500"), "expected '500' in: {msg}");
    }

    #[test]
    fn budget_stop_wall_clock_timeout_message() {
        let msg = BudgetStop::WallClockTimeout {
            limit: Duration::from_secs(120),
        }
        .message();
        assert!(
            msg.to_lowercase().contains("timeout"),
            "expected 'timeout' in: {msg}"
        );
    }

    #[test]
    fn budget_stop_idle_progress_timeout_message() {
        let msg = BudgetStop::IdleProgressTimeout {
            limit: Duration::from_secs(120),
        }
        .message();
        assert!(
            msg.to_lowercase().contains("idle"),
            "expected 'idle' in: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("progress"),
            "expected 'progress' in: {msg}"
        );
    }

    // ---------- ConversationResponse derives ----------

    #[test]
    fn conversation_response_clone_and_debug() {
        use super::super::ConversationResponse;

        let resp = ConversationResponse {
            content: "test".into(),
            reasoning_content: None,
            provider_metadata: None,
            token_usage: octos_core::TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            files_modified: vec![],
            streamed: false,
            messages: vec![],
        };
        let cloned = resp.clone();
        assert_eq!(cloned.content, "test");
        assert_eq!(cloned.token_usage.input_tokens, 10);

        // Debug trait works
        let debug = format!("{:?}", cloned);
        assert!(debug.contains("ConversationResponse"));
    }
}
