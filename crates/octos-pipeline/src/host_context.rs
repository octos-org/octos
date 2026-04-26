//! M8 parity: snapshot of the parent session's shared resources picked
//! up via [`octos_agent::tools::TOOL_CTX`] when a `run_pipeline` tool
//! call enters the executor.
//!
//! The pipeline historically constructed sub-agents in isolation —
//! every node opened its own `FileStateCache`, no
//! `SubAgentOutputRouter`, no `TaskSupervisor` registration, no shared
//! cost ledger. M8 runtime parity (issue #592 — track W1) closes that
//! gap by snapshotting the live values from the parent session's
//! `TOOL_CTX` at the start of the run and threading them down through
//! [`crate::executor::ExecutorConfig`] onto every node worker.
//!
//! The fields are all optional so:
//! * Pipelines invoked outside a session (unit tests, CLI) keep their
//!   pre-M8 behaviour byte-for-byte.
//! * A test can build a synthetic context with a custom router /
//!   supervisor and assert wiring without requiring a real session
//!   actor.

use std::sync::Arc;

use octos_agent::cost_ledger::CostAccountant;
use octos_agent::file_state_cache::FileStateCache;
use octos_agent::subagent_output::SubAgentOutputRouter;
use octos_agent::subagent_summary::AgentSummaryGenerator;
use octos_agent::task_supervisor::TaskSupervisor;
use octos_agent::tools::ToolContext;

/// Shared resources inherited from the parent session by a pipeline
/// run. Each field is independently optional so legacy callers stay on
/// the no-op path when the field is unset.
#[derive(Clone, Default)]
pub struct PipelineHostContext {
    /// Parent session's [`FileStateCache`]. When set, every pipeline
    /// node worker is built with `Agent::with_file_state_cache(...)` so
    /// file tools see the same cache state as the foreground turn.
    pub file_state_cache: Option<Arc<FileStateCache>>,
    /// Parent session's [`SubAgentOutputRouter`]. Threaded onto every
    /// pipeline node worker so background output is routed through the
    /// shared on-disk router, not a per-node copy.
    pub subagent_output_router: Option<Arc<SubAgentOutputRouter>>,
    /// Parent session's [`AgentSummaryGenerator`]. When set, pipeline
    /// nodes that go background can emit periodic summaries through
    /// the same generator the parent session uses.
    pub subagent_summary_generator: Option<Arc<AgentSummaryGenerator>>,
    /// Parent session's [`TaskSupervisor`]. Pipeline nodes register a
    /// child task here on dispatch so the admin dashboard sees the
    /// substructure under the `run_pipeline` parent invocation
    /// (W1.A3).
    pub task_supervisor: Option<Arc<TaskSupervisor>>,
    /// Parent session's [`CostAccountant`]. Per-node reservations open
    /// against this accountant so spend rolls up under the parent
    /// session contract instead of a fresh pipeline-local ledger
    /// (W1.A4).
    pub cost_accountant: Option<Arc<CostAccountant>>,
    /// `tool_call_id` of the `run_pipeline` invocation. Threaded
    /// through to every node task as the `parent_task_id` so the UI
    /// can stitch the node tree under the invoking tool-call pill
    /// (W1.A3).
    pub parent_tool_call_id: Option<String>,
    /// Owning session key, when known. Recorded on per-node task
    /// registrations so the supervisor links the child task to the
    /// owning session.
    pub parent_session_key: Option<String>,
}

impl PipelineHostContext {
    /// Snapshot the host context from the active task-local
    /// [`ToolContext`]. Cheap (only `Arc::clone` per field).
    pub fn from_tool_context(ctx: &ToolContext) -> Self {
        Self {
            file_state_cache: ctx.file_state_cache.clone(),
            subagent_output_router: ctx.subagent_output_router.clone(),
            subagent_summary_generator: ctx.subagent_summary_generator.clone(),
            task_supervisor: ctx.task_supervisor.clone(),
            cost_accountant: ctx.cost_accountant.clone(),
            parent_tool_call_id: if ctx.tool_id.is_empty() {
                None
            } else {
                Some(ctx.tool_id.clone())
            },
            parent_session_key: ctx.parent_session_key.clone(),
        }
    }

    /// Returns `true` when no field was populated. Used by the
    /// executor's wiring tests to assert legacy callers stay on the
    /// pre-M8 path.
    pub fn is_empty(&self) -> bool {
        self.file_state_cache.is_none()
            && self.subagent_output_router.is_none()
            && self.subagent_summary_generator.is_none()
            && self.task_supervisor.is_none()
            && self.cost_accountant.is_none()
            && self.parent_tool_call_id.is_none()
            && self.parent_session_key.is_none()
    }
}

impl std::fmt::Debug for PipelineHostContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineHostContext")
            .field("file_state_cache", &self.file_state_cache.is_some())
            .field(
                "subagent_output_router",
                &self.subagent_output_router.is_some(),
            )
            .field(
                "subagent_summary_generator",
                &self.subagent_summary_generator.is_some(),
            )
            .field("task_supervisor", &self.task_supervisor.is_some())
            .field("cost_accountant", &self.cost_accountant.is_some())
            .field("parent_tool_call_id", &self.parent_tool_call_id)
            .field("parent_session_key", &self.parent_session_key)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_default_is_empty() {
        let ctx = PipelineHostContext::default();
        assert!(ctx.is_empty());
    }

    #[test]
    fn from_zero_tool_context_is_empty() {
        let tool_ctx = ToolContext::zero();
        let host = PipelineHostContext::from_tool_context(&tool_ctx);
        assert!(host.is_empty());
        assert!(host.parent_tool_call_id.is_none());
    }

    #[test]
    fn from_tool_context_picks_up_tool_id() {
        let mut tool_ctx = ToolContext::zero();
        tool_ctx.tool_id = "tool-call-42".into();
        let host = PipelineHostContext::from_tool_context(&tool_ctx);
        assert_eq!(host.parent_tool_call_id.as_deref(), Some("tool-call-42"));
    }

    #[test]
    fn from_tool_context_picks_up_file_state_cache() {
        let mut tool_ctx = ToolContext::zero();
        tool_ctx.file_state_cache = Some(Arc::new(FileStateCache::new()));
        let host = PipelineHostContext::from_tool_context(&tool_ctx);
        assert!(host.file_state_cache.is_some());
        assert!(!host.is_empty());
    }
}
