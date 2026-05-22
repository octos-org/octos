//! Swarm-local glue around the shared dispatch-policy gate.
//!
//! The gate type and enforcement logic live in
//! [`octos_agent::dispatch_policy`] — both
//! [`crate::dispatcher::Swarm::dispatch_once`] and the
//! [`octos_agent::tools::SpawnTool`] `agent_mcp` branch route through
//! the same checks so a single bypass cannot reopen the audit's #701
//! / #714 finding.
//!
//! This module keeps a swarm-side adapter ([`enforce_or_outcome`])
//! that folds a [`octos_agent::GateDenial`] into a swarm-local
//! [`crate::SubtaskOutcome`] so the existing event / metrics path
//! does not have to learn the agent crate's failure type.

use octos_agent::tools::mcp_agent::McpAgentBackend;
use octos_agent::{DispatchPolicy, DispatchTarget, enforce_dispatch_gates};

use crate::result::{SubtaskOutcome, SubtaskStatus};
use crate::topology::ContractSpec;

/// Helper for the dispatcher: enforce gates and synthesise a
/// [`SubtaskOutcome`] on denial. Returns `Ok(())` if dispatch may
/// proceed, otherwise the failure outcome to use in place of a backend
/// dispatch.
pub(crate) async fn enforce_or_outcome(
    policy: &DispatchPolicy,
    backend: &dyn McpAgentBackend,
    contract: &ContractSpec,
    prior_attempts: u32,
) -> std::result::Result<(), SubtaskOutcome> {
    let target = DispatchTarget {
        dispatch_id: &contract.contract_id,
        tool_name: &contract.tool_name,
        task: &contract.task,
    };
    match enforce_dispatch_gates(policy, backend, target).await {
        Ok(()) => Ok(()),
        Err(denial) => Err(SubtaskOutcome {
            contract_id: contract.contract_id.clone(),
            label: contract.label.clone(),
            status: SubtaskStatus::TerminalFailed,
            attempts: prior_attempts.saturating_add(1),
            last_dispatch_outcome: denial.last_dispatch_outcome.to_string(),
            output: denial.reason.clone(),
            files_to_send: Vec::new(),
            error: Some(denial.reason),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use octos_agent::ToolPolicy;
    use octos_agent::tools::mcp_agent::{DispatchOutcome, DispatchRequest, DispatchResponse};

    struct StubBackend;

    #[async_trait]
    impl McpAgentBackend for StubBackend {
        fn backend_label(&self) -> &'static str {
            "local"
        }
        fn endpoint_label(&self) -> String {
            "stub".into()
        }
        async fn dispatch(&self, _request: DispatchRequest) -> DispatchResponse {
            DispatchResponse {
                outcome: DispatchOutcome::Success,
                output: String::new(),
                files_to_send: Vec::new(),
                error: None,
                context_contract: None,
            }
        }
    }

    fn contract(id: &str, tool: &str) -> ContractSpec {
        ContractSpec {
            contract_id: id.into(),
            tool_name: tool.into(),
            task: serde_json::json!({}),
            label: None,
        }
    }

    #[tokio::test]
    async fn enforce_or_outcome_passes_when_policy_is_noop() {
        let policy = DispatchPolicy::default();
        let backend = StubBackend;
        let contract = contract("c1", "any_tool");
        assert!(
            enforce_or_outcome(&policy, &backend, &contract, 0)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn enforce_or_outcome_folds_tool_policy_denial_into_subtask_outcome() {
        let policy = DispatchPolicy {
            tool_policy: Some(ToolPolicy {
                deny: vec!["forbidden".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let backend = StubBackend;
        let contract = contract("c1", "forbidden");
        let outcome = enforce_or_outcome(&policy, &backend, &contract, 0)
            .await
            .expect_err("denied");
        assert_eq!(outcome.status, SubtaskStatus::TerminalFailed);
        assert_eq!(outcome.last_dispatch_outcome, "policy_denied");
        assert_eq!(outcome.attempts, 1);
        assert!(outcome.error.unwrap().contains("forbidden"));
    }
}
