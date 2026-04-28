//! Workspace-contract context for pipeline execution (coding-blue FA-7).
//!
//! The pipeline engine was historically contract-oblivious: every node
//! dispatched a sub-agent without inheriting the parent's compaction
//! policy, validator rail, or cost ledger. The [`PipelineContext`] type
//! and [`PipelineExecutor::with_workspace_context`] builder close that
//! gap while keeping the legacy [`PipelineExecutor::new`] path bitwise
//! identical to today.
//!
//! Callers that opt in (site-delivery, slides-delivery, and any future
//! background workflow) pass:
//! * an optional [`WorkspacePolicy`] — compaction block propagates onto
//!   every LLM-call node, validator block runs at pipeline terminal and
//!   optionally per-node via `validators_by_node`;
//! * an optional [`LlmProvider`] — the agent provider used to construct
//!   `CompactionRunner::with_provider(...)` when a node declares
//!   LLM-iterative summarisation;
//! * an optional [`CostAccountant`] — pipeline-level reservation at
//!   dispatch start + per-node sub-reservations for LLM-call nodes.
//!
//! Design invariants (see supervisor brief):
//! * When `policy` is `None` the engine stays on the legacy path —
//!   matches pre-FA-7 behaviour byte-for-byte.
//! * Human gates never spend tokens or reserve budget (the handler is
//!   [`HandlerKind::Gate`] and carries no model metadata).
//! * Checkpoints do not compact (they are not LLM calls).
//! * Conditional branches that never run do not consume reservations —
//!   dropped [`ReservationHandle`]s auto-refund via their `Drop` impl.
//!
//! FA-7 consumes the [`CompactionRunner::with_provider`] (FA-1) and
//! [`CostAccountant::reserve`] (FA-3) APIs — this module never modifies
//! the octos-agent contract surface.

use std::collections::BTreeMap;
use std::sync::Arc;

use octos_agent::cost_ledger::CostAccountant;
use octos_agent::workspace_policy::{Validator, WorkspacePolicy};
use octos_llm::LlmProvider;

/// Per-node validator overrides for pipelines.
///
/// Maps a pipeline node id to the validator block that should run after
/// that node completes. The pipeline-terminal run is independent and
/// driven by [`WorkspacePolicy::validation.on_completion`] +
/// `validators`; `validators_by_node` only fires at per-node granularity
/// and is intended for workflows that gate an intermediate artifact
/// (e.g. "validate the design file before moving to generation").
pub type ValidatorsByNode = BTreeMap<String, Vec<Validator>>;

/// Opt-in workspace contract context threaded into the pipeline
/// executor.
///
/// Populated via [`crate::PipelineExecutor::with_workspace_context`].
/// Each field is independently optional so a caller that only wants
/// compaction (but no cost reservation) can set just `policy` +
/// `agent_llm_provider` and leave `cost_accountant` as `None`.
#[derive(Clone, Default)]
pub struct PipelineContext {
    /// Declarative workspace policy (validators + compaction +
    /// artifacts). Absent = legacy pipeline behaviour.
    pub policy: Option<WorkspacePolicy>,
    /// Agent LLM provider used to construct
    /// `CompactionRunner::with_provider(...)` for LLM-iterative
    /// summarisation. Ignored when `policy.compaction.summarizer` is
    /// `Extractive`.
    pub agent_llm_provider: Option<Arc<dyn LlmProvider>>,
    /// Shared cost ledger + reservation table. When present, the
    /// executor reserves a pipeline-level projection at dispatch start
    /// and a sub-reservation per LLM-call node; on success the
    /// pipeline-level handle commits the cumulative spend.
    pub cost_accountant: Option<Arc<CostAccountant>>,
    /// Per-node validator overrides. Empty = only the pipeline-terminal
    /// run fires.
    pub validators_by_node: ValidatorsByNode,
    /// Logical contract id for the cost ledger. Defaults to `"pipeline"`
    /// so legacy callers that supply a bare `CostAccountant` without
    /// setting this field still get a consistent rollup key.
    pub contract_id: String,
    /// Projected USD cost for the pipeline at dispatch start. Callers
    /// are free to estimate based on declared models + expected node
    /// count; if unset we default to a trivial `0.001` projection that
    /// still exercises the reservation path so budget-policy errors
    /// surface early.
    pub pipeline_projected_usd: f64,
}

impl PipelineContext {
    /// Build an empty context — equivalent to passing `None` to
    /// [`crate::PipelineExecutor::with_workspace_context`] but handy
    /// for tests that want to toggle individual fields.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a workspace policy. Returns `self` for builder chaining.
    pub fn with_policy(mut self, policy: WorkspacePolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Attach the agent LLM provider used for LLM-iterative compaction.
    pub fn with_agent_llm_provider(mut self, provider: Arc<dyn LlmProvider>) -> Self {
        self.agent_llm_provider = Some(provider);
        self
    }

    /// Attach a cost accountant. The first dispatch reserves
    /// `pipeline_projected_usd` against the configured `contract_id`.
    pub fn with_cost_accountant(mut self, accountant: Arc<CostAccountant>) -> Self {
        self.cost_accountant = Some(accountant);
        self
    }

    /// Install per-node validator overrides.
    pub fn with_validators_by_node(mut self, validators: ValidatorsByNode) -> Self {
        self.validators_by_node = validators;
        self
    }

    /// Set the logical contract id used for cost-ledger rollups.
    pub fn with_contract_id(mut self, contract_id: impl Into<String>) -> Self {
        self.contract_id = contract_id.into();
        self
    }

    /// Set the pipeline-level projected USD at dispatch start.
    pub fn with_projected_usd(mut self, projected_usd: f64) -> Self {
        self.pipeline_projected_usd = projected_usd;
        self
    }

    /// Returns `true` when no field was set — used by the executor to
    /// early-return to the legacy path so we don't even look up the
    /// compaction block on workloads that opted out.
    pub fn is_empty(&self) -> bool {
        self.policy.is_none()
            && self.agent_llm_provider.is_none()
            && self.cost_accountant.is_none()
            && self.validators_by_node.is_empty()
    }
}

impl std::fmt::Debug for PipelineContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineContext")
            .field("policy_present", &self.policy.is_some())
            .field("agent_llm_present", &self.agent_llm_provider.is_some())
            .field("cost_accountant_present", &self.cost_accountant.is_some())
            .field(
                "validators_by_node_keys",
                &self.validators_by_node.keys().collect::<Vec<_>>(),
            )
            .field("contract_id", &self.contract_id)
            .field("pipeline_projected_usd", &self.pipeline_projected_usd)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_agent::WORKSPACE_POLICY_SCHEMA_VERSION;
    use octos_agent::workspace_policy::{
        ValidationPolicy, ValidatorPhaseKind, ValidatorSpec, WorkspacePolicy, WorkspacePolicyKind,
    };
    use octos_agent::workspace_policy::{
        WorkspaceArtifactsPolicy, WorkspacePolicyWorkspace, WorkspaceSnapshotTrigger,
        WorkspaceTrackingPolicy, WorkspaceVersionControlPolicy, WorkspaceVersionControlProvider,
    };

    fn empty_policy() -> WorkspacePolicy {
        WorkspacePolicy {
            schema_version: WORKSPACE_POLICY_SCHEMA_VERSION,
            workspace: WorkspacePolicyWorkspace {
                kind: WorkspacePolicyKind::Session,
            },
            version_control: WorkspaceVersionControlPolicy {
                provider: WorkspaceVersionControlProvider::Git,
                auto_init: false,
                trigger: WorkspaceSnapshotTrigger::TurnEnd,
                fail_on_error: false,
            },
            tracking: WorkspaceTrackingPolicy { ignore: Vec::new() },
            validation: ValidationPolicy::default(),
            artifacts: WorkspaceArtifactsPolicy::default(),
            spawn_tasks: BTreeMap::new(),
            compaction: None,
        }
    }

    #[test]
    fn default_context_is_empty() {
        let ctx = PipelineContext::new();
        assert!(ctx.is_empty());
        assert!(ctx.policy.is_none());
        assert!(ctx.agent_llm_provider.is_none());
        assert!(ctx.cost_accountant.is_none());
        assert!(ctx.validators_by_node.is_empty());
    }

    #[test]
    fn with_policy_sets_policy_and_breaks_empty_guard() {
        let policy = empty_policy();
        let ctx = PipelineContext::new().with_policy(policy);
        assert!(!ctx.is_empty());
        assert!(ctx.policy.is_some());
    }

    #[test]
    fn with_validators_by_node_tracks_override_map() {
        let mut overrides: ValidatorsByNode = BTreeMap::new();
        overrides.insert(
            "design".into(),
            vec![Validator {
                id: "design-file".into(),
                required: true,
                timeout_ms: None,
                phase: ValidatorPhaseKind::Completion,
                spec: ValidatorSpec::FileExists {
                    path: "design.md".into(),
                    min_bytes: None,
                },
            }],
        );
        let ctx = PipelineContext::new().with_validators_by_node(overrides);
        assert!(!ctx.is_empty());
        assert_eq!(ctx.validators_by_node.len(), 1);
        assert!(ctx.validators_by_node.contains_key("design"));
    }

    #[test]
    fn with_contract_id_and_projection_are_builder_independent() {
        let ctx = PipelineContext::new()
            .with_contract_id("slides-delivery")
            .with_projected_usd(0.25);
        assert!(ctx.is_empty(), "scalars alone don't flip is_empty");
        assert_eq!(ctx.contract_id, "slides-delivery");
        assert!((ctx.pipeline_projected_usd - 0.25).abs() < f64::EPSILON);
    }
}
