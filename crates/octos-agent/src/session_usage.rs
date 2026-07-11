//! Session-cumulative usage shared between an embedder and the agent.
//!
//! The agent's per-turn accounting (`LoopTurnState`) resets on every
//! `run_conversation` call, so the `session_*` figures it emits on
//! `ProgressEvent::CostUpdate` used to cover only the CURRENT turn — and
//! were re-priced wholesale at whichever model produced the latest
//! response. A multi-model session (provider failover today, and
//! `profile/llm/select` switching once the runtime cache evicts and
//! rebuilds the agent) therefore never saw a truthful cumulative figure:
//! each turn's spend vanished from the display when the next turn began.
//!
//! `SessionUsageHandle` is the cross-turn base. The embedder that owns the
//! session (the gateway/serve session actor) creates one per session, seeds
//! it from the persistent usage ledger so it survives agent rebuilds and
//! process restarts, injects it via [`Agent::with_session_usage`], and folds
//! each completed run back in — priced at the model that ran it. The agent
//! only ever READS it (`snapshot`) when emitting cost updates: emission =
//! base (completed runs, per-model priced) + live turn (per-response
//! priced). Without an injected handle the agent behaves as before, minus
//! the re-pricing: emission covers just the live turn.
//!
//! [`Agent::with_session_usage`]: crate::Agent::with_session_usage

use std::sync::{Arc, RwLock};

/// Point-in-time view of a session's cumulative usage across completed runs.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct SessionUsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Sum of per-run estimated costs, each priced at the model that
    /// produced the run. Runs whose model had no catalog pricing
    /// contribute tokens but no spend.
    pub spend_usd: f64,
    /// How many folded runs carried a price. Zero means `spend_usd` is
    /// vacuous — emission uses this to keep the cost line hidden instead
    /// of showing a misleading `$0.0000` for never-priced sessions.
    pub priced_runs: u64,
}

/// Shared, thread-safe session usage accumulator. See the module docs for
/// the ownership contract (embedder writes, agent reads).
#[derive(Debug, Default)]
pub struct SessionUsageHandle {
    inner: RwLock<SessionUsageSnapshot>,
}

impl SessionUsageHandle {
    pub fn snapshot(&self) -> SessionUsageSnapshot {
        *self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Replace the accumulated state wholesale — used once at session
    /// start to hydrate from the persistent usage ledger.
    pub fn seed(&self, snapshot: SessionUsageSnapshot) {
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
    }

    /// Fold one completed run into the base. `estimated_cost_usd` is the
    /// run's spend priced at the model that ran it; `None` means pricing
    /// was unavailable for that model.
    pub fn fold_run(&self, input_tokens: u64, output_tokens: u64, estimated_cost_usd: Option<f64>) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.input_tokens = inner.input_tokens.saturating_add(input_tokens);
        inner.output_tokens = inner.output_tokens.saturating_add(output_tokens);
        if let Some(cost) = estimated_cost_usd {
            inner.spend_usd += cost;
            inner.priced_runs = inner.priced_runs.saturating_add(1);
        }
    }
}

/// The sharing unit: one per session, cloned into the agent at build time.
pub type SharedSessionUsage = Arc<SessionUsageHandle>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accumulate_tokens_and_spend_when_runs_fold() {
        let handle = SessionUsageHandle::default();
        handle.fold_run(100, 40, Some(0.5));
        handle.fold_run(200, 60, Some(0.25));
        let snap = handle.snapshot();
        assert_eq!(snap.input_tokens, 300);
        assert_eq!(snap.output_tokens, 100);
        assert!((snap.spend_usd - 0.75).abs() < f64::EPSILON);
        assert_eq!(snap.priced_runs, 2);
    }

    #[test]
    fn should_count_tokens_but_not_priced_runs_when_cost_unknown() {
        let handle = SessionUsageHandle::default();
        handle.fold_run(100, 40, None);
        let snap = handle.snapshot();
        assert_eq!(snap.input_tokens, 100);
        assert_eq!(snap.priced_runs, 0);
        assert!((snap.spend_usd - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn should_replace_state_when_seeded_from_ledger() {
        let handle = SessionUsageHandle::default();
        handle.fold_run(1, 1, Some(0.1));
        handle.seed(SessionUsageSnapshot {
            input_tokens: 5000,
            output_tokens: 700,
            spend_usd: 1.25,
            priced_runs: 3,
        });
        let snap = handle.snapshot();
        assert_eq!(snap.input_tokens, 5000);
        assert_eq!(snap.priced_runs, 3);
        // Folds after a seed keep accumulating on top of it.
        handle.fold_run(1000, 300, Some(0.75));
        let snap = handle.snapshot();
        assert_eq!(snap.input_tokens, 6000);
        assert_eq!(snap.output_tokens, 1000);
        assert!((snap.spend_usd - 2.0).abs() < f64::EPSILON);
        assert_eq!(snap.priced_runs, 4);
    }
}
