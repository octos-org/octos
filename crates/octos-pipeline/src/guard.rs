//! In-process pipeline guard hooks.

use std::collections::HashMap;
use std::time::Duration;

use eyre::Result;

use crate::graph::{PipelineGraph, PipelineNode};

/// Read-only state exposed to [`PipelineGuard::before_node`].
pub struct GuardContext<'a> {
    /// Pipeline graph being executed.
    pub graph: &'a PipelineGraph,
    /// Node about to be dispatched.
    pub node: &'a PipelineNode,
    /// Cumulative input + output tokens spent by completed nodes.
    pub cumulative_tokens: u32,
    /// Elapsed wall-clock time since pipeline execution started.
    pub elapsed: Duration,
    /// Number of node outcomes recorded so far.
    pub completed_count: usize,
    /// Per-node visit counts. The current node has already been counted.
    pub visit_counts: &'a HashMap<String, usize>,
}

/// Decision returned by a guard before a node dispatches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Continue evaluating subsequent guards, then dispatch the node.
    Allow,
    /// Mark the node as `Fail` and route edges from that synthetic outcome.
    Skip(String),
    /// Stop the pipeline and return the partial result collected so far.
    Abort(String),
}

/// A synchronous in-process hook evaluated before pipeline node dispatch.
pub trait PipelineGuard: Send + Sync {
    /// Decide whether the next node may run.
    ///
    /// Returning `Err` fails the pipeline immediately. Guard errors are
    /// never treated as `Allow`, so safety checks cannot be silently disabled.
    fn before_node(&self, ctx: &GuardContext<'_>) -> Result<GuardDecision>;
}

/// Abort when the cumulative pipeline token spend reaches a fixed ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudgetGuard {
    max_tokens: u32,
}

impl TokenBudgetGuard {
    pub fn new(max_tokens: u32) -> Self {
        Self { max_tokens }
    }

    pub fn max_tokens(&self) -> u32 {
        self.max_tokens
    }
}

impl PipelineGuard for TokenBudgetGuard {
    fn before_node(&self, ctx: &GuardContext<'_>) -> Result<GuardDecision> {
        if ctx.cumulative_tokens >= self.max_tokens {
            Ok(GuardDecision::Abort(format!(
                "token budget exhausted before node '{}': spent {}/{} tokens",
                ctx.node.id, ctx.cumulative_tokens, self.max_tokens
            )))
        } else {
            Ok(GuardDecision::Allow)
        }
    }
}

/// Abort when pipeline wall-clock runtime reaches a fixed timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutGuard {
    timeout: Duration,
}

impl TimeoutGuard {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl PipelineGuard for TimeoutGuard {
    fn before_node(&self, ctx: &GuardContext<'_>) -> Result<GuardDecision> {
        if ctx.elapsed >= self.timeout {
            Ok(GuardDecision::Abort(format!(
                "pipeline timeout before node '{}': elapsed {:?} >= {:?}",
                ctx.node.id, ctx.elapsed, self.timeout
            )))
        } else {
            Ok(GuardDecision::Allow)
        }
    }
}
