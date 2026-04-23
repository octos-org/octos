//! Swarm topology types and contract specs.
//!
//! The [`Swarm::dispatch`](crate::Swarm::dispatch) call accepts a list of
//! [`ContractSpec`] + a [`SwarmTopology`]. Topology controls how the
//! primitive issues sub-contracts to the underlying [`octos_agent::tools::mcp_agent::McpAgentBackend`]:
//!
//! - [`SwarmTopology::Parallel`]: fan out up to `n` concurrent sub-contracts,
//!   aggregate in arrival order.
//! - [`SwarmTopology::Sequential`]: run sub-contracts one at a time, abort on
//!   the first hard (non-retryable) failure.
//! - [`SwarmTopology::Pipeline`]: chain outputs, feeding the artifact of
//!   contract `i` as structured input into contract `i + 1`.
//! - [`SwarmTopology::Fanout`]: same fan-out shape as `Parallel`, but the
//!   sub-contracts are derived from a typed [`FanoutPattern`] expanded at
//!   dispatch time.
//!
//! Every topology variant is serde-friendly so it can persist alongside the
//! redb-backed dispatch state and round-trip across process restart.

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

/// The maximum number of contracts a single swarm dispatch may issue.
/// Enforced at [`Swarm::dispatch`](crate::Swarm::dispatch) entry so a
/// runaway topology expansion cannot unbounded-spawn sub-agents.
pub const MAX_CONTRACTS_PER_DISPATCH: usize = 128;

/// A single contract the primitive hands to an [`McpAgentBackend`] via
/// `tools/call`. The `contract_id` is the stable key the primitive uses to
/// deduplicate retries and persist per-subtask state — callers MUST
/// guarantee it is unique within a dispatch.
///
/// The `task` payload is opaque to the primitive: it is forwarded verbatim
/// as the MCP `tools/call` arguments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContractSpec {
    /// Stable identifier unique within the dispatch. Used as the redb
    /// subtask key so idempotent reruns converge on the same row.
    pub contract_id: String,
    /// MCP tool name to invoke on the sub-agent (for example
    /// `claude_code/run_task`).
    pub tool_name: String,
    /// Structured task payload — forwarded verbatim as `tools/call`
    /// arguments.
    pub task: serde_json::Value,
    /// Optional human-readable label for operator UIs. Not used for
    /// correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Pattern describing how to expand a single seed contract into N
/// sibling sub-contracts for [`SwarmTopology::Fanout`]. Kept simple on
/// purpose: the primitive stamps a `variant` field on each expanded
/// contract's `task` payload so the remote agent can switch behaviour
/// without the primitive inspecting the payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FanoutPattern {
    /// The contract template that is cloned N times.
    pub seed: ContractSpec,
    /// Distinct variant labels appended to each expanded contract's id
    /// and injected into the task payload at key `variant`.
    pub variants: Vec<String>,
}

impl FanoutPattern {
    /// Expand the pattern into concrete per-variant contracts. The
    /// variant label is appended to the seed's `contract_id` (separated
    /// by `::`) so the primitive can persist each expansion distinctly
    /// in the redb subtask table.
    pub fn expand(&self) -> Vec<ContractSpec> {
        self.variants
            .iter()
            .map(|variant| {
                let mut task = self.seed.task.clone();
                if let serde_json::Value::Object(ref mut obj) = task {
                    obj.insert("variant".into(), serde_json::Value::String(variant.clone()));
                }
                ContractSpec {
                    contract_id: format!("{}::{variant}", self.seed.contract_id),
                    tool_name: self.seed.tool_name.clone(),
                    task,
                    label: self
                        .seed
                        .label
                        .as_ref()
                        .map(|base| format!("{base} ({variant})")),
                }
            })
            .collect()
    }
}

/// Topology controlling how the primitive issues sub-contracts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SwarmTopology {
    /// Fan out up to `max_concurrency` contracts in parallel. Aggregation
    /// is in arrival order — the first sub-contract to finish is the
    /// first to land in the result.
    Parallel { max_concurrency: NonZeroUsize },
    /// Run sub-contracts one at a time. The dispatch aborts on the first
    /// hard (non-retryable) sub-contract failure and surfaces the
    /// partial result.
    Sequential,
    /// Chain sub-contracts — the output of contract `i` is folded into
    /// the task payload of contract `i + 1` at key `pipeline_input`.
    Pipeline,
    /// Expand a [`FanoutPattern`] then run the expansion in parallel
    /// with the given max concurrency.
    Fanout {
        pattern: FanoutPattern,
        max_concurrency: NonZeroUsize,
    },
}

impl SwarmTopology {
    /// Stable topology label used in metrics and typed events.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Parallel { .. } => "parallel",
            Self::Sequential => "sequential",
            Self::Pipeline => "pipeline",
            Self::Fanout { .. } => "fanout",
        }
    }

    /// Returns the effective list of contracts this topology produces
    /// given a seed list. [`Parallel`], [`Sequential`] and [`Pipeline`]
    /// preserve the caller's list; [`Fanout`] ignores the caller's list
    /// and expands the embedded pattern.
    pub fn resolve_contracts(&self, seed: &[ContractSpec]) -> Vec<ContractSpec> {
        match self {
            Self::Fanout { pattern, .. } => pattern.expand(),
            _ => seed.to_vec(),
        }
    }

    /// Max concurrent sub-contracts for this topology. [`Sequential`]
    /// and [`Pipeline`] always return 1; the fan-out variants honour
    /// their configured concurrency cap.
    pub fn max_concurrency(&self) -> usize {
        match self {
            Self::Parallel { max_concurrency }
            | Self::Fanout {
                max_concurrency, ..
            } => max_concurrency.get(),
            Self::Sequential | Self::Pipeline => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_labels_are_stable() {
        assert_eq!(
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(4).unwrap()
            }
            .as_str(),
            "parallel"
        );
        assert_eq!(SwarmTopology::Sequential.as_str(), "sequential");
        assert_eq!(SwarmTopology::Pipeline.as_str(), "pipeline");
        let pattern = FanoutPattern {
            seed: ContractSpec {
                contract_id: "seed".into(),
                tool_name: "run".into(),
                task: serde_json::json!({}),
                label: None,
            },
            variants: vec!["a".into(), "b".into()],
        };
        assert_eq!(
            SwarmTopology::Fanout {
                pattern,
                max_concurrency: NonZeroUsize::new(2).unwrap()
            }
            .as_str(),
            "fanout"
        );
    }

    #[test]
    fn fanout_expands_variant_count() {
        let pattern = FanoutPattern {
            seed: ContractSpec {
                contract_id: "seed".into(),
                tool_name: "run".into(),
                task: serde_json::json!({"base": "x"}),
                label: Some("seed".into()),
            },
            variants: vec!["a".into(), "b".into(), "c".into()],
        };
        let expanded = pattern.expand();
        assert_eq!(expanded.len(), 3);
        assert_eq!(expanded[0].contract_id, "seed::a");
        assert_eq!(expanded[1].contract_id, "seed::b");
        assert_eq!(expanded[0].task["variant"], "a");
        assert_eq!(expanded[0].task["base"], "x");
        assert_eq!(expanded[0].label.as_deref(), Some("seed (a)"));
    }

    #[test]
    fn resolve_preserves_seed_for_non_fanout() {
        let seed = vec![ContractSpec {
            contract_id: "c1".into(),
            tool_name: "run".into(),
            task: serde_json::json!({}),
            label: None,
        }];
        let topo = SwarmTopology::Parallel {
            max_concurrency: NonZeroUsize::new(1).unwrap(),
        };
        let resolved = topo.resolve_contracts(&seed);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].contract_id, "c1");
    }

    #[test]
    fn sequential_has_unit_concurrency() {
        assert_eq!(SwarmTopology::Sequential.max_concurrency(), 1);
        assert_eq!(SwarmTopology::Pipeline.max_concurrency(), 1);
    }
}
