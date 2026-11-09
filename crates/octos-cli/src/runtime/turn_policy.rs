//! Execution intent, not the transport, selects the implicit turn limit.
//!
//! Human-driven OUP, chat and ACP turns share the agent's unlimited default.
//! Autonomous channel actors and continuations retain a finite backstop.
//! Explicit operator limits (including zero) always win. Convergence
//! checkpoints are independent: reaching one reflects and continues.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnIntent {
    #[cfg(feature = "api")]
    Interactive,
    Autonomous,
}

pub(crate) const AUTONOMOUS_MAX_ITERATIONS: u32 = 50;

pub(crate) fn max_iterations(configured: Option<u32>, intent: TurnIntent) -> u32 {
    if let Some(limit) = configured {
        return limit;
    }
    match intent {
        #[cfg(feature = "api")]
        TurnIntent::Interactive => octos_agent::AgentConfig::default().max_iterations,
        TurnIntent::Autonomous => AUTONOMOUS_MAX_ITERATIONS,
    }
}
