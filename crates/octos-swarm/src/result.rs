//! Typed outcome for a swarm dispatch.
//!
//! [`SwarmResult`] is returned by [`Swarm::dispatch`](crate::Swarm::dispatch)
//! and captures everything a supervisor needs to render a post-dispatch
//! report: per-subtask provenance, the aggregate artifact, the M4.3
//! validator outcomes, and the rolled-up cost.

use std::collections::HashMap;
use std::path::PathBuf;

use octos_agent::tools::mcp_agent::DispatchOutcome;
use octos_agent::validators::ValidatorOutcome;
use serde::{Deserialize, Serialize};

use crate::topology::SwarmTopology;

/// Coarse status of a single sub-contract after the retry loop finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtaskStatus {
    /// The sub-contract reached a successful terminal state.
    Completed,
    /// The sub-contract failed and is a candidate for re-dispatch.
    RetryableFailed,
    /// The sub-contract failed hard and must not be re-dispatched
    /// (transport error, SSRF block, protocol error).
    TerminalFailed,
}

impl SubtaskStatus {
    /// Stable label for metrics and event payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::RetryableFailed => "retryable_failed",
            Self::TerminalFailed => "terminal_failed",
        }
    }

    /// Map the MCP [`DispatchOutcome`] to a swarm-level status. Only
    /// [`DispatchOutcome::Success`] counts as completed. Remote /
    /// timeout errors are retryable; transport, protocol and SSRF
    /// errors are terminal and the primitive will not re-dispatch them.
    pub fn from_dispatch(outcome: DispatchOutcome) -> Self {
        match outcome {
            DispatchOutcome::Success => Self::Completed,
            DispatchOutcome::RemoteError | DispatchOutcome::Timeout => Self::RetryableFailed,
            DispatchOutcome::TransportError
            | DispatchOutcome::ProtocolError
            | DispatchOutcome::SsrfBlocked => Self::TerminalFailed,
        }
    }
}

/// Per-subtask dispatch provenance, persisted in the redb session state
/// and surfaced in [`SwarmResult::per_task_outcomes`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubtaskOutcome {
    pub contract_id: String,
    /// Human-readable label for operator UIs (copied from the contract
    /// spec). Not used for correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub status: SubtaskStatus,
    /// Number of attempts (1-indexed). First attempt + retries <= bound.
    pub attempts: u32,
    /// Final dispatch outcome label from the MCP backend, or `"not_run"`
    /// if the retry budget was exhausted before we reached it.
    pub last_dispatch_outcome: String,
    /// Plain-text summary the sub-agent returned on its final attempt.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
    /// Files the sub-agent declared as artifacts. The primitive folds
    /// these through the workspace contract before surfacing them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_to_send: Vec<PathBuf>,
    /// Optional error message for non-completed states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SubtaskOutcome {
    pub(crate) fn pending(contract_id: impl Into<String>, label: Option<String>) -> Self {
        Self {
            contract_id: contract_id.into(),
            label,
            status: SubtaskStatus::RetryableFailed,
            attempts: 0,
            last_dispatch_outcome: "not_run".to_string(),
            output: String::new(),
            files_to_send: Vec::new(),
            error: None,
        }
    }
}

/// Aggregate artifact produced by the swarm. This is the flattened
/// view a supervisor renders or feeds into a downstream pipeline step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AggregateArtifact {
    /// Concatenated outputs from every completed sub-contract. Order
    /// matches [`SwarmResult::per_task_outcomes`] (arrival order for
    /// `Parallel` / `Fanout`, declared order for `Sequential` /
    /// `Pipeline`).
    pub combined_output: String,
    /// Every file path declared by a completed sub-contract.
    pub combined_files: Vec<PathBuf>,
    /// Free-form metadata callers can stash on the aggregate (routing
    /// info, supervisor notes). Never interpreted by the primitive.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Coarse aggregate outcome used in metrics + the typed event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmOutcomeKind {
    /// Every sub-contract completed successfully and the aggregate
    /// validator passed (or no validators were configured).
    Success,
    /// Some sub-contracts completed, some remained failed after the
    /// retry budget. The aggregate is still useful; caller may decide
    /// to re-dispatch externally.
    Partial,
    /// No sub-contract completed. Dispatch is a total loss.
    Failed,
    /// Sequential topology aborted on a hard failure before the list
    /// was exhausted.
    Aborted,
}

impl SwarmOutcomeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

/// Full result of a swarm dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwarmResult {
    /// Stable dispatch id (persists across process restart).
    pub dispatch_id: String,
    /// Coarse aggregate outcome.
    pub outcome: SwarmOutcomeKind,
    /// Topology used (serialized representation).
    pub topology: String,
    /// Total number of sub-contracts issued.
    pub total_subtasks: u32,
    /// How many of them completed successfully.
    pub completed_subtasks: u32,
    /// Retry round count actually consumed (0 = no retries).
    pub retry_rounds_used: u32,
    /// Per-subtask provenance, in aggregation order.
    pub per_task_outcomes: Vec<SubtaskOutcome>,
    /// Flattened aggregate artifact.
    pub aggregate_artifact: AggregateArtifact,
    /// Outcomes from the M4.3 aggregate validator (if configured).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validator_results: Vec<ValidatorOutcome>,
    /// Rolled-up cost from the cost ledger. `None` until M7.4 lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
}

impl SwarmResult {
    pub(crate) fn from_parts(
        dispatch_id: String,
        topology: &SwarmTopology,
        per_task_outcomes: Vec<SubtaskOutcome>,
        validator_results: Vec<ValidatorOutcome>,
        total_cost_usd: Option<f64>,
        retry_rounds_used: u32,
    ) -> Self {
        let total_subtasks = per_task_outcomes.len() as u32;
        let completed_subtasks = per_task_outcomes
            .iter()
            .filter(|outcome| outcome.status == SubtaskStatus::Completed)
            .count() as u32;
        // Sequential/Pipeline abort if a terminal failure surfaced
        // before the list was exhausted — any `not_run` slot after a
        // hard failure is the tell-tale signal.
        let aborted = matches!(
            topology,
            SwarmTopology::Sequential | SwarmTopology::Pipeline
        ) && per_task_outcomes
            .iter()
            .any(|outcome| outcome.status == SubtaskStatus::TerminalFailed)
            && per_task_outcomes
                .iter()
                .any(|outcome| outcome.last_dispatch_outcome == "not_run");
        let validator_failed = validator_results
            .iter()
            .any(|result| !result.required_gate_passed());

        let outcome =
            if completed_subtasks == total_subtasks && total_subtasks > 0 && !validator_failed {
                SwarmOutcomeKind::Success
            } else if aborted {
                SwarmOutcomeKind::Aborted
            } else if completed_subtasks == 0 && total_subtasks > 0 {
                SwarmOutcomeKind::Failed
            } else {
                SwarmOutcomeKind::Partial
            };

        let aggregate_artifact = build_aggregate(&per_task_outcomes);

        Self {
            dispatch_id,
            outcome,
            topology: topology.as_str().to_string(),
            total_subtasks,
            completed_subtasks,
            retry_rounds_used,
            per_task_outcomes,
            aggregate_artifact,
            validator_results,
            total_cost_usd,
        }
    }
}

fn build_aggregate(outcomes: &[SubtaskOutcome]) -> AggregateArtifact {
    let mut combined_output = String::new();
    let mut combined_files = Vec::new();
    for outcome in outcomes {
        if outcome.status != SubtaskStatus::Completed {
            continue;
        }
        if !combined_output.is_empty() {
            combined_output.push('\n');
        }
        combined_output.push_str(&outcome.output);
        combined_files.extend(outcome.files_to_send.iter().cloned());
    }
    AggregateArtifact {
        combined_output,
        combined_files,
        extra: HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_agent::tools::mcp_agent::DispatchOutcome;
    use std::num::NonZeroUsize;

    #[test]
    fn subtask_status_maps_dispatch_outcomes() {
        assert_eq!(
            SubtaskStatus::from_dispatch(DispatchOutcome::Success),
            SubtaskStatus::Completed
        );
        assert_eq!(
            SubtaskStatus::from_dispatch(DispatchOutcome::Timeout),
            SubtaskStatus::RetryableFailed
        );
        assert_eq!(
            SubtaskStatus::from_dispatch(DispatchOutcome::RemoteError),
            SubtaskStatus::RetryableFailed
        );
        assert_eq!(
            SubtaskStatus::from_dispatch(DispatchOutcome::TransportError),
            SubtaskStatus::TerminalFailed
        );
        assert_eq!(
            SubtaskStatus::from_dispatch(DispatchOutcome::SsrfBlocked),
            SubtaskStatus::TerminalFailed
        );
    }

    #[test]
    fn from_parts_rolls_outcome_to_success_when_all_complete() {
        let outcomes = vec![SubtaskOutcome {
            contract_id: "c1".into(),
            label: None,
            status: SubtaskStatus::Completed,
            attempts: 1,
            last_dispatch_outcome: "success".into(),
            output: "hello".into(),
            files_to_send: vec![],
            error: None,
        }];
        let topo = SwarmTopology::Parallel {
            max_concurrency: NonZeroUsize::new(1).unwrap(),
        };
        let result = SwarmResult::from_parts("d1".into(), &topo, outcomes, vec![], None, 0);
        assert_eq!(result.outcome, SwarmOutcomeKind::Success);
        assert_eq!(result.completed_subtasks, 1);
        assert_eq!(result.aggregate_artifact.combined_output, "hello");
    }

    #[test]
    fn from_parts_rolls_outcome_to_partial_when_mixed() {
        let outcomes = vec![
            SubtaskOutcome {
                contract_id: "c1".into(),
                label: None,
                status: SubtaskStatus::Completed,
                attempts: 1,
                last_dispatch_outcome: "success".into(),
                output: "done".into(),
                files_to_send: vec![],
                error: None,
            },
            SubtaskOutcome {
                contract_id: "c2".into(),
                label: None,
                status: SubtaskStatus::RetryableFailed,
                attempts: 4,
                last_dispatch_outcome: "timeout".into(),
                output: String::new(),
                files_to_send: vec![],
                error: Some("timeout".into()),
            },
        ];
        let topo = SwarmTopology::Parallel {
            max_concurrency: NonZeroUsize::new(2).unwrap(),
        };
        let result = SwarmResult::from_parts("d2".into(), &topo, outcomes, vec![], None, 3);
        assert_eq!(result.outcome, SwarmOutcomeKind::Partial);
        assert_eq!(result.completed_subtasks, 1);
        assert_eq!(result.total_subtasks, 2);
    }

    #[test]
    fn from_parts_surfaces_aborted_for_sequential_hard_fail() {
        // Invariant 3: sequential topology aborts on first terminal
        // failure — the tell-tale is a terminal failure PLUS a
        // trailing `not_run` placeholder for the untouched contracts.
        let outcomes = vec![
            SubtaskOutcome {
                contract_id: "c1".into(),
                label: None,
                status: SubtaskStatus::TerminalFailed,
                attempts: 1,
                last_dispatch_outcome: "transport_error".into(),
                output: String::new(),
                files_to_send: vec![],
                error: Some("transport error".into()),
            },
            SubtaskOutcome::pending("c2", None),
        ];
        let result = SwarmResult::from_parts(
            "d3".into(),
            &SwarmTopology::Sequential,
            outcomes,
            vec![],
            None,
            0,
        );
        assert_eq!(result.outcome, SwarmOutcomeKind::Aborted);
    }

    #[test]
    fn aggregate_skips_failed_subtasks() {
        let outcomes = vec![
            SubtaskOutcome {
                contract_id: "c1".into(),
                label: None,
                status: SubtaskStatus::Completed,
                attempts: 1,
                last_dispatch_outcome: "success".into(),
                output: "good".into(),
                files_to_send: vec![PathBuf::from("/tmp/a")],
                error: None,
            },
            SubtaskOutcome {
                contract_id: "c2".into(),
                label: None,
                status: SubtaskStatus::RetryableFailed,
                attempts: 4,
                last_dispatch_outcome: "timeout".into(),
                output: "bad".into(),
                files_to_send: vec![PathBuf::from("/tmp/b")],
                error: Some("timeout".into()),
            },
        ];
        let topo = SwarmTopology::Parallel {
            max_concurrency: NonZeroUsize::new(2).unwrap(),
        };
        let result = SwarmResult::from_parts("d4".into(), &topo, outcomes, vec![], None, 3);
        assert_eq!(result.aggregate_artifact.combined_output, "good");
        assert_eq!(
            result.aggregate_artifact.combined_files,
            vec![PathBuf::from("/tmp/a")]
        );
    }
}
