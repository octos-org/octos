//! Execution intent, not the transport, selects the implicit turn limit.
//!
//! Human-driven OUP, chat and ACP turns share the agent's unlimited default.
//! Autonomous channel actors and continuations retain a finite backstop.
//! Explicit operator limits (including zero) always win. Convergence
//! checkpoints are independent: reaching one reflects and continues.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnIntent {
    #[cfg(any(feature = "api", test))]
    Interactive,
    Autonomous,
}

pub(crate) const AUTONOMOUS_MAX_ITERATIONS: u32 = 50;

pub(crate) fn max_iterations(configured: Option<u32>, intent: TurnIntent) -> u32 {
    if let Some(limit) = configured {
        return limit;
    }
    match intent {
        #[cfg(any(feature = "api", test))]
        TurnIntent::Interactive => octos_agent::AgentConfig::default().max_iterations,
        TurnIntent::Autonomous => AUTONOMOUS_MAX_ITERATIONS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_turns_do_not_inherit_a_transport_cap() {
        assert_eq!(max_iterations(None, TurnIntent::Interactive), 0);
        assert_eq!(
            max_iterations(None, TurnIntent::Interactive),
            octos_agent::AgentConfig::default().max_iterations
        );
    }

    #[test]
    fn unattended_backstop_and_explicit_limits_remain_intact() {
        assert_eq!(max_iterations(None, TurnIntent::Autonomous), 50);
        for intent in [TurnIntent::Interactive, TurnIntent::Autonomous] {
            for limit in [0, 1, 5, 120] {
                assert_eq!(max_iterations(Some(limit), intent), limit);
            }
        }
    }
}
