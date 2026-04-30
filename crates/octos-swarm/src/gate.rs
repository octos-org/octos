//! Pre-dispatch policy gate for [`Swarm::dispatch`](crate::Swarm::dispatch).
//!
//! Closes M7 requirement 7 (policy enforcement parity): every backend the
//! swarm dispatches to — local stdio MCP, remote HTTP MCP, native sub-agent
//! via [`octos_agent::tools::SpawnTool`] — is funnelled through
//! [`crate::dispatcher::Swarm::dispatch_once`]. This module wires the same
//! gates the native [`octos_agent::tools::ToolRegistry::execute_with_context`]
//! path applies (tool policy, approval, sandbox, env allowlist) before any
//! `McpAgentBackend::dispatch` call.
//!
//! The gate is **opt-in**: callers wire it via
//! [`crate::SwarmBuilder::with_dispatch_policy`]. Without a configured
//! policy the dispatcher's behaviour is unchanged so existing M7.1 callers
//! and tests do not regress.
//!
//! ## Failure surfacing
//!
//! Each gate failure synthesises a [`crate::SubtaskOutcome`] with status
//! `TerminalFailed` and a stable `last_dispatch_outcome` label. The label
//! flows through the existing `octos_swarm_dispatch_total{topology,outcome}`
//! counter and the typed
//! [`octos_agent::HarnessEventPayload::SwarmDispatch`] event the harness
//! observability channel consumes (M7 requirement 8 stays satisfied).
//!
//! Stable labels:
//!
//! - `policy_denied` — [`octos_agent::ToolPolicy`] denied the contract's
//!   tool name. The `error` carries the policy reason
//!   (`policy_deny` or `robot_tier_gate`).
//! - `approval_denied` — the configured
//!   [`octos_agent::ToolApprovalRequester`] returned
//!   [`octos_agent::ToolApprovalDecision::Deny`].
//! - `approval_unavailable` — approval is required but no requester was
//!   wired. **Fail closed** — never fall through to dispatch.
//! - `env_forbidden` — the contract's task carries an env key that fails
//!   the dispatch policy's env allowlist (used by callers that pass env
//!   through the task payload to the backend).
//! - `sandbox_required` — the dispatch policy demands a sandboxed
//!   backend but the wired backend does not self-report sandboxing.

use std::collections::HashSet;
use std::sync::Arc;

use octos_agent::tools::mcp_agent::McpAgentBackend;
use octos_agent::{
    PolicyDecision, ToolApprovalDecision, ToolApprovalRequest, ToolApprovalRequester, ToolPolicy,
};
use tracing::warn;

use crate::result::{SubtaskOutcome, SubtaskStatus};
use crate::topology::ContractSpec;

/// Configuration for the swarm dispatch policy gate.
///
/// Every field is independently optional so callers can opt into just the
/// gates they need. An entirely default value is a no-op (matches the
/// pre-fix dispatcher behaviour).
#[derive(Clone, Default)]
pub struct DispatchPolicy {
    /// Tool policy evaluated against [`ContractSpec::tool_name`]. Deny
    /// wins; an empty allow list permits every tool name not explicitly
    /// denied (mirrors [`ToolPolicy`] semantics).
    pub tool_policy: Option<ToolPolicy>,
    /// When `true`, every dispatch must clear the approval gate before
    /// the backend is called. The dispatch fails closed if no
    /// [`ToolApprovalRequester`] is wired.
    pub require_approval: bool,
    /// Approval bridge used when [`Self::require_approval`] is true.
    pub approval_requester: Option<Arc<dyn ToolApprovalRequester>>,
    /// Env keys the dispatch is allowed to forward to the backend.
    /// Inspected against the contract's task payload — if the task
    /// carries an `env` object whose keys overlap any name **not** in
    /// this allowlist, the dispatch is denied with `env_forbidden`.
    /// `None` means env-checking is off (existing
    /// [`octos_agent::tools::mcp_agent::StdioMcpAgent`] env handling
    /// remains the only barrier). Names are matched case-insensitively
    /// against the upper-cased form (mirrors
    /// `subprocess_env::EnvAllowlist`).
    pub env_allowlist: Option<HashSet<String>>,
    /// When `true`, the wired backend must self-report as sandboxed. No
    /// [`McpAgentBackend`] does today, so this field is provided for
    /// forward compatibility — setting it true on a non-sandboxed
    /// backend fails closed every time.
    pub require_sandboxed: bool,
}

impl std::fmt::Debug for DispatchPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchPolicy")
            .field("tool_policy", &self.tool_policy)
            .field("require_approval", &self.require_approval)
            .field(
                "approval_requester",
                &self.approval_requester.as_ref().map(|_| "<requester>"),
            )
            .field("env_allowlist", &self.env_allowlist)
            .field("require_sandboxed", &self.require_sandboxed)
            .finish()
    }
}

impl DispatchPolicy {
    /// True if every gate is unset — the dispatcher can skip the
    /// gate entirely.
    pub fn is_noop(&self) -> bool {
        self.tool_policy
            .as_ref()
            .is_none_or(|policy| policy.is_empty())
            && !self.require_approval
            && self.env_allowlist.is_none()
            && !self.require_sandboxed
    }
}

/// Outcome produced when a gate denies a dispatch. Folded directly into
/// a [`SubtaskOutcome`] so the swarm result and harness event surface
/// the failure with the same shape as backend failures.
#[derive(Debug, Clone)]
pub(crate) struct GateDenial {
    pub last_dispatch_outcome: &'static str,
    pub reason: String,
}

impl GateDenial {
    fn into_outcome(self, contract: &ContractSpec, prior_attempts: u32) -> SubtaskOutcome {
        SubtaskOutcome {
            contract_id: contract.contract_id.clone(),
            label: contract.label.clone(),
            status: SubtaskStatus::TerminalFailed,
            attempts: prior_attempts.saturating_add(1),
            last_dispatch_outcome: self.last_dispatch_outcome.to_string(),
            output: self.reason.clone(),
            files_to_send: Vec::new(),
            error: Some(self.reason),
        }
    }
}

/// Run every configured gate against `contract`. Returns `Ok(())` if the
/// dispatch may proceed, otherwise the first failing gate's denial.
///
/// Gates run in this fixed order, so the surfaced failure matches the
/// most-deterministic check first:
///
/// 1. Sandbox requirement (cheapest, config-only).
/// 2. Tool policy (synchronous evaluator, no I/O).
/// 3. Env allowlist (synchronous, inspects the task payload).
/// 4. Approval (last; may block on user interaction).
pub(crate) async fn enforce_dispatch_gates(
    policy: &DispatchPolicy,
    backend: &dyn McpAgentBackend,
    contract: &ContractSpec,
) -> std::result::Result<(), GateDenial> {
    if policy.is_noop() {
        return Ok(());
    }

    if policy.require_sandboxed && !backend_is_sandboxed(backend) {
        return Err(GateDenial {
            last_dispatch_outcome: "sandbox_required",
            reason: format!(
                "swarm dispatch requires a sandboxed backend; backend '{}' (endpoint '{}') is not sandboxed",
                backend.backend_label(),
                backend.endpoint_label()
            ),
        });
    }

    if let Some(ref tool_policy) = policy.tool_policy {
        if let PolicyDecision::Deny { reason } = tool_policy.evaluate(&contract.tool_name) {
            warn!(
                contract_id = %contract.contract_id,
                tool_name = %contract.tool_name,
                deny_reason = reason,
                "swarm dispatch denied by tool policy"
            );
            return Err(GateDenial {
                last_dispatch_outcome: "policy_denied",
                reason: format!(
                    "tool '{}' denied by swarm dispatch policy ({})",
                    contract.tool_name, reason
                ),
            });
        }
    }

    if let Some(ref allowlist) = policy.env_allowlist {
        if let Some(forbidden) = first_forbidden_env_key(&contract.task, allowlist) {
            warn!(
                contract_id = %contract.contract_id,
                forbidden_key = %forbidden,
                "swarm dispatch denied by env allowlist"
            );
            return Err(GateDenial {
                last_dispatch_outcome: "env_forbidden",
                reason: format!(
                    "env variable '{forbidden}' is not in the swarm dispatch allowlist"
                ),
            });
        }
    }

    if policy.require_approval {
        let Some(ref requester) = policy.approval_requester else {
            warn!(
                contract_id = %contract.contract_id,
                "swarm dispatch requires approval but no approver is wired"
            );
            return Err(GateDenial {
                last_dispatch_outcome: "approval_unavailable",
                reason: format!(
                    "swarm dispatch policy requires approval but no requester is wired (contract '{}')",
                    contract.contract_id
                ),
            });
        };
        let request = ToolApprovalRequest {
            tool_id: contract.contract_id.clone(),
            tool_name: contract.tool_name.clone(),
            title: format!("Approve swarm dispatch for {}", contract.tool_name),
            body: format!(
                "Backend '{}' (endpoint '{}') will receive contract '{}'.",
                backend.backend_label(),
                backend.endpoint_label(),
                contract.contract_id
            ),
            command: None,
            cwd: None,
        };
        let decision = requester.request_approval(request).await;
        if matches!(decision, ToolApprovalDecision::Deny) {
            warn!(
                contract_id = %contract.contract_id,
                tool_name = %contract.tool_name,
                "swarm dispatch denied by approval requester"
            );
            return Err(GateDenial {
                last_dispatch_outcome: "approval_denied",
                reason: format!(
                    "swarm dispatch for tool '{}' denied by approval requester",
                    contract.tool_name
                ),
            });
        }
    }

    Ok(())
}

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
    match enforce_dispatch_gates(policy, backend, contract).await {
        Ok(()) => Ok(()),
        Err(denial) => Err(denial.into_outcome(contract, prior_attempts)),
    }
}

fn backend_is_sandboxed(_backend: &dyn McpAgentBackend) -> bool {
    // No `McpAgentBackend` implementation reports as sandboxed today.
    // The trait does not expose an `is_sandboxed()` method, so callers
    // that demand sandboxing must wire a backend that wraps the dispatch
    // call site in their own isolation surface (Bubblewrap subprocess,
    // Docker container, etc.). When the trait grows an `is_sandboxed()`
    // method, this helper becomes the single switch-point.
    false
}

fn first_forbidden_env_key(
    task: &serde_json::Value,
    allowlist: &HashSet<String>,
) -> Option<String> {
    let env = task.get("env")?.as_object()?;
    for key in env.keys() {
        let normalized = key.to_ascii_uppercase();
        if !allowlist.contains(&normalized) {
            return Some(key.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
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
    async fn noop_policy_passes_every_dispatch() {
        let policy = DispatchPolicy::default();
        let backend = StubBackend;
        let contract = contract("c1", "any_tool");
        assert!(
            enforce_dispatch_gates(&policy, &backend, &contract)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn tool_policy_deny_blocks_dispatch() {
        let tool_policy = ToolPolicy {
            deny: vec!["forbidden".into()],
            ..Default::default()
        };
        let policy = DispatchPolicy {
            tool_policy: Some(tool_policy),
            ..Default::default()
        };
        let backend = StubBackend;
        let contract = contract("c1", "forbidden");
        let denial = enforce_dispatch_gates(&policy, &backend, &contract)
            .await
            .expect_err("denied");
        assert_eq!(denial.last_dispatch_outcome, "policy_denied");
        assert!(denial.reason.contains("forbidden"));
    }

    #[tokio::test]
    async fn tool_policy_allowlist_misses_block_dispatch() {
        let policy = DispatchPolicy {
            tool_policy: Some(ToolPolicy {
                allow: vec!["allowed_tool".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let backend = StubBackend;
        let contract = contract("c1", "another_tool");
        let denial = enforce_dispatch_gates(&policy, &backend, &contract)
            .await
            .expect_err("denied");
        assert_eq!(denial.last_dispatch_outcome, "policy_denied");
    }

    #[tokio::test]
    async fn approval_required_without_requester_fails_closed() {
        let policy = DispatchPolicy {
            require_approval: true,
            ..Default::default()
        };
        let backend = StubBackend;
        let contract = contract("c1", "any_tool");
        let denial = enforce_dispatch_gates(&policy, &backend, &contract)
            .await
            .expect_err("denied");
        assert_eq!(denial.last_dispatch_outcome, "approval_unavailable");
    }

    #[tokio::test]
    async fn approval_deny_blocks_dispatch() {
        struct DenyRequester;

        #[async_trait]
        impl ToolApprovalRequester for DenyRequester {
            async fn request_approval(&self, _: ToolApprovalRequest) -> ToolApprovalDecision {
                ToolApprovalDecision::Deny
            }
        }

        let policy = DispatchPolicy {
            require_approval: true,
            approval_requester: Some(Arc::new(DenyRequester)),
            ..Default::default()
        };
        let backend = StubBackend;
        let contract = contract("c1", "any_tool");
        let denial = enforce_dispatch_gates(&policy, &backend, &contract)
            .await
            .expect_err("denied");
        assert_eq!(denial.last_dispatch_outcome, "approval_denied");
    }

    #[tokio::test]
    async fn approval_approve_passes_through() {
        struct ApproveRequester;

        #[async_trait]
        impl ToolApprovalRequester for ApproveRequester {
            async fn request_approval(&self, _: ToolApprovalRequest) -> ToolApprovalDecision {
                ToolApprovalDecision::Approve
            }
        }

        let policy = DispatchPolicy {
            require_approval: true,
            approval_requester: Some(Arc::new(ApproveRequester)),
            ..Default::default()
        };
        let backend = StubBackend;
        let contract = contract("c1", "any_tool");
        assert!(
            enforce_dispatch_gates(&policy, &backend, &contract)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn env_allowlist_rejects_forbidden_keys() {
        let mut allowlist = HashSet::new();
        allowlist.insert("OPENAI_API_KEY".to_string());

        let policy = DispatchPolicy {
            env_allowlist: Some(allowlist),
            ..Default::default()
        };
        let backend = StubBackend;
        let contract = ContractSpec {
            contract_id: "c1".into(),
            tool_name: "any".into(),
            task: serde_json::json!({"env": {"LD_PRELOAD": "/tmp/evil.so"}}),
            label: None,
        };
        let denial = enforce_dispatch_gates(&policy, &backend, &contract)
            .await
            .expect_err("denied");
        assert_eq!(denial.last_dispatch_outcome, "env_forbidden");
        assert!(denial.reason.contains("LD_PRELOAD"));
    }

    #[tokio::test]
    async fn env_allowlist_passes_allowed_keys() {
        let mut allowlist = HashSet::new();
        allowlist.insert("OPENAI_API_KEY".to_string());

        let policy = DispatchPolicy {
            env_allowlist: Some(allowlist),
            ..Default::default()
        };
        let backend = StubBackend;
        let contract = ContractSpec {
            contract_id: "c1".into(),
            tool_name: "any".into(),
            task: serde_json::json!({"env": {"OPENAI_API_KEY": "sk-test"}}),
            label: None,
        };
        assert!(
            enforce_dispatch_gates(&policy, &backend, &contract)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn require_sandboxed_blocks_unsandboxed_backend() {
        let policy = DispatchPolicy {
            require_sandboxed: true,
            ..Default::default()
        };
        let backend = StubBackend;
        let contract = contract("c1", "any_tool");
        let denial = enforce_dispatch_gates(&policy, &backend, &contract)
            .await
            .expect_err("denied");
        assert_eq!(denial.last_dispatch_outcome, "sandbox_required");
    }

    #[test]
    fn is_noop_handles_default_and_empty_policy() {
        assert!(DispatchPolicy::default().is_noop());
        assert!(
            DispatchPolicy {
                tool_policy: Some(ToolPolicy::default()),
                ..Default::default()
            }
            .is_noop()
        );
    }
}
