//! Pre-dispatch policy gate shared by every MCP-agent dispatch site —
//! [`crate::tools::SpawnTool`]'s `agent_mcp` branch (#714) and the
//! [`octos_swarm::Swarm`] dispatcher (#710 / #713).
//!
//! Pre-#714, [`crate::tools::SpawnTool`] dispatched to its configured
//! MCP backend via [`crate::tools::mcp_agent::dispatch_with_metrics`]
//! with no policy gate at all — even when `octos serve` had wired one
//! into the swarm side, the direct spawn path was a bypass. Lifting
//! the gate out of `octos-swarm` and into this crate lets both call
//! sites enforce the same shape of gates against the same shared
//! types ([`crate::ToolPolicy`], [`crate::ToolApprovalRequester`],
//! [`crate::tools::mcp_agent::McpAgentBackend`]).
//!
//! [`DispatchPolicy`] **exposes** the same shape of gates the native
//! [`crate::ToolRegistry::execute_with_context`] path applies: tool
//! policy, approval, sandbox-required, and env (allowlist or
//! denylist). Whether a given gate is *active* is up to the caller —
//! see [`DispatchPolicy::from_agent_gates`] for the production
//! constructor `octos serve` uses (it wires `tool_policy` +
//! injection-env denylist; approval bridge / sandbox-required /
//! per-skill manifest env are intentionally not mirrored, see that
//! constructor's rustdoc for the full boundary).
//!
//! The gate is **opt-in**: callers wire it via
//! [`crate::tools::SpawnTool::with_dispatch_policy`] or
//! [`octos_swarm::SwarmBuilder::with_dispatch_policy`]. Without a
//! configured policy the dispatcher's behaviour is unchanged so
//! existing M7.1 callers and tests do not regress.
//!
//! ## Failure surfacing
//!
//! Each gate failure produces a [`GateDenial`] with a stable
//! `last_dispatch_outcome` label. Stable labels:
//!
//! - `policy_denied` — [`crate::ToolPolicy`] denied the dispatched
//!   tool name. The `error` carries the policy reason
//!   (`policy_deny` or `robot_tier_gate`).
//! - `approval_denied` — the configured
//!   [`crate::ToolApprovalRequester`] returned
//!   [`crate::ToolApprovalDecision::Deny`].
//! - `approval_unavailable` — approval is required but no requester
//!   was wired. **Fail closed** — never fall through to dispatch.
//! - `env_forbidden` — the dispatch's task payload or the backend's
//!   configured env carries a key that fails either the dispatch
//!   policy's env allowlist (key not in allowlist) or env denylist
//!   (key in denylist).
//! - `sandbox_required` — the dispatch policy demands a sandboxed
//!   backend but the wired backend does not self-report sandboxing.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::warn;

use crate::tools::mcp_agent::McpAgentBackend;
use crate::{
    PolicyDecision, ToolApprovalDecision, ToolApprovalRequest, ToolApprovalRequester, ToolPolicy,
};

/// Backend facts needed by the dispatch policy gate.
///
/// The gate only needs display labels for diagnostics, whether the
/// caller can prove the dispatch will run under a sandbox, and the env
/// keys the backend will set on the spawned child (#1601). Keeping
/// this metadata independent from [`McpAgentBackend`] lets direct CLI
/// or native-specialist launchers reuse the same gate instead of
/// open-coding parallel checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchBackendMetadata {
    backend_label: String,
    endpoint_label: String,
    sandboxed: bool,
    /// Env keys the backend is configured to set on the spawned child
    /// ([#1601](https://github.com/octos-org/octos/issues/1601)). The
    /// env gate inspects them alongside any `env` object on the
    /// dispatch payload.
    env_keys: Vec<String>,
}

impl DispatchBackendMetadata {
    pub fn new(
        backend_label: impl Into<String>,
        endpoint_label: impl Into<String>,
        sandboxed: bool,
    ) -> Self {
        Self {
            backend_label: backend_label.into(),
            endpoint_label: endpoint_label.into(),
            sandboxed,
            env_keys: Vec::new(),
        }
    }

    pub fn unsandboxed(
        backend_label: impl Into<String>,
        endpoint_label: impl Into<String>,
    ) -> Self {
        Self::new(backend_label, endpoint_label, false)
    }

    pub fn sandboxed(backend_label: impl Into<String>, endpoint_label: impl Into<String>) -> Self {
        Self::new(backend_label, endpoint_label, true)
    }

    pub fn from_mcp_backend(backend: &dyn McpAgentBackend) -> Self {
        Self::unsandboxed(backend.backend_label(), backend.endpoint_label())
            .with_env_keys(backend.configured_env_keys())
    }

    /// Record the env keys the backend will set on the spawned child so
    /// the env gates can inspect them.
    pub fn with_env_keys(mut self, env_keys: Vec<String>) -> Self {
        self.env_keys = env_keys;
        self
    }

    pub fn backend_label(&self) -> &str {
        &self.backend_label
    }

    pub fn endpoint_label(&self) -> &str {
        &self.endpoint_label
    }

    pub fn is_sandboxed(&self) -> bool {
        self.sandboxed
    }

    pub fn env_keys(&self) -> &[String] {
        &self.env_keys
    }
}

/// Configuration for the dispatch policy gate.
///
/// Every field is independently optional so callers can opt into just the
/// gates they need. An entirely default value is a no-op (matches the
/// pre-#710/#714 dispatcher behaviour).
#[derive(Clone, Default)]
pub struct DispatchPolicy {
    /// Tool policy evaluated against the dispatched tool name. Deny
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
    /// Inspected against the dispatched task payload's `env` object
    /// and the backend's configured env keys — a key overlapping any
    /// name **not** in this allowlist denies the dispatch with
    /// `env_forbidden`. `None` means allowlist checking is off. Keys
    /// are matched after upper-casing, so entries must be stored
    /// upper-cased (as [`Self::from_agent_gates`] does).
    pub env_allowlist: Option<HashSet<String>>,
    /// Env keys the dispatch must reject. Complements
    /// [`Self::env_allowlist`]: an entry here is denied unconditionally
    /// (matches the agent's `subprocess_env::BLOCKED_ENV_VARS` denylist
    /// semantics, e.g. `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`,
    /// `NODE_OPTIONS`). Inspected against both the task payload's `env`
    /// object and the backend's configured env keys. Both fields can be
    /// wired together — the denylist runs first so a permissive
    /// allowlist cannot accidentally let a known-bad key through. Keys
    /// are matched after upper-casing, so entries must be stored
    /// upper-cased (as [`Self::from_agent_gates`] does). `None` means
    /// the denylist gate is off.
    pub env_denylist: Option<HashSet<String>>,
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
            .field("env_denylist", &self.env_denylist)
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
            && self
                .env_denylist
                .as_ref()
                .is_none_or(|denylist| denylist.is_empty())
            && !self.require_sandboxed
    }

    /// M7 req 7 production wiring (#713 / #714): build a
    /// [`DispatchPolicy`] that inherits the **two operator-configured
    /// gates** the audit's #701 finding called out — the workspace-wide
    /// tool-name policy (`config.tool_policy`) and the shared
    /// `BLOCKED_ENV_VARS` injection-env denylist — so MCP and CLI
    /// swarm backends fail closed on the same names native execution
    /// rejects, without requiring operators to wire a separate config
    /// file.
    ///
    /// **Not mirrored by this constructor** (intentional, scope of
    /// #713 / #701):
    ///
    /// - `require_approval` / `approval_requester` — the native
    ///   approval bridge is `TOOL_APPROVAL_CTX`, a per-turn
    ///   task-local; there is no global requester to clone at server
    ///   startup. Operators that want dispatch-level approval can
    ///   layer it on by mutating the public fields after construction.
    /// - `require_sandboxed` — no [`McpAgentBackend`] self-reports as
    ///   sandboxed today; the field is forward-compat.
    /// - `env_allowlist` — the native side uses denylist semantics
    ///   (drop blocked names) plus secret-detection, not an allowlist.
    ///   This constructor populates the parallel `env_denylist` field
    ///   so the gate semantics match.
    /// - Per-skill manifest env allowlists — those live on the
    ///   plugin tool, not the workspace config; they are out of scope
    ///   for a workspace-level dispatch gate.
    ///
    /// `tool_policy: None` keeps the tool-name gate off (matches the
    /// native side: an absent config means no policy is applied).
    /// `block_injection_env_vars: true` populates the env denylist
    /// with the workspace-wide [`crate::sandbox::BLOCKED_ENV_VARS`]
    /// set so dispatches fail closed if the contract carries — or the
    /// backend is configured to set (#1601) — `LD_PRELOAD`,
    /// `DYLD_INSERT_LIBRARIES`, `NODE_OPTIONS`, etc.
    /// Use `false` only for tests that explicitly need to drive an
    /// injection-style env through the gate.
    pub fn from_agent_gates(
        tool_policy: Option<ToolPolicy>,
        block_injection_env_vars: bool,
    ) -> Self {
        let env_denylist = if block_injection_env_vars {
            Some(
                crate::sandbox::BLOCKED_ENV_VARS
                    .iter()
                    .map(|name| name.to_ascii_uppercase())
                    .collect::<HashSet<String>>(),
            )
        } else {
            None
        };
        Self {
            tool_policy,
            require_approval: false,
            approval_requester: None,
            env_allowlist: None,
            env_denylist,
            require_sandboxed: false,
        }
    }
}

/// Outcome produced when a gate denies a dispatch. Callers fold this
/// into their crate-local outcome shape (swarm `SubtaskOutcome`,
/// spawn `ToolResult`, etc.) so the surface area on this crate's API
/// is the gate semantics, not the failure type.
#[derive(Debug, Clone)]
pub struct GateDenial {
    /// Stable label for the dispatch metric / harness event. One of:
    /// `policy_denied`, `approval_denied`, `approval_unavailable`,
    /// `env_forbidden`, `sandbox_required`.
    pub last_dispatch_outcome: &'static str,
    /// Human-readable reason for the failure. Safe to expose to the
    /// caller — does not embed sensitive payload values.
    pub reason: String,
}

/// Minimal shape of the dispatch a caller wants to gate. The native
/// swarm dispatcher passes its `ContractSpec`; the spawn `agent_mcp`
/// path passes the constructed dispatch payload. Holding only the
/// fields the gate actually inspects keeps this crate free of the
/// swarm-only `ContractSpec` type.
pub struct DispatchTarget<'a> {
    /// Stable identifier for log / metric correlation. The swarm side
    /// uses the contract id; the spawn side uses the child task id.
    pub dispatch_id: &'a str,
    /// Tool name evaluated against [`DispatchPolicy::tool_policy`].
    pub tool_name: &'a str,
    /// Task payload inspected for forbidden env keys when an
    /// allowlist / denylist is configured. The gate also inspects the
    /// backend's configured env keys ([`DispatchBackendMetadata::env_keys`])
    /// so a policy cannot be bypassed by setting injection-class env in
    /// the backend config instead of the payload (#1601).
    pub task: &'a serde_json::Value,
}

/// Run every configured gate against `target`. Returns `Ok(())` if the
/// dispatch may proceed, otherwise the first failing gate's denial.
///
/// Gates run in this fixed order, so the surfaced failure matches the
/// most-deterministic check first:
///
/// 1. Sandbox requirement (cheapest, config-only).
/// 2. Tool policy (synchronous evaluator, no I/O).
/// 3. Env denylist (synchronous, inspects the payload `env` object and
///    the backend's configured env keys — runs before the allowlist so
///    a permissive allowlist cannot let a known-bad key through).
/// 4. Env allowlist (synchronous, same inputs).
/// 5. Approval (last; may block on user interaction).
pub async fn enforce_dispatch_gates(
    policy: &DispatchPolicy,
    backend: &dyn McpAgentBackend,
    target: DispatchTarget<'_>,
) -> Result<(), GateDenial> {
    let metadata = DispatchBackendMetadata::from_mcp_backend(backend);
    enforce_dispatch_gates_for_backend(policy, &metadata, target).await
}

/// Same policy gate as [`enforce_dispatch_gates`], but parameterized by
/// backend metadata so non-MCP launchers do not bypass the central path.
pub async fn enforce_dispatch_gates_for_backend(
    policy: &DispatchPolicy,
    backend: &DispatchBackendMetadata,
    target: DispatchTarget<'_>,
) -> Result<(), GateDenial> {
    if policy.is_noop() {
        return Ok(());
    }

    if policy.require_sandboxed && !backend.is_sandboxed() {
        return Err(GateDenial {
            last_dispatch_outcome: "sandbox_required",
            reason: format!(
                "dispatch requires a sandboxed backend; backend '{}' (endpoint '{}') is not sandboxed",
                backend.backend_label(),
                backend.endpoint_label()
            ),
        });
    }

    if let Some(ref tool_policy) = policy.tool_policy {
        if let PolicyDecision::Deny { reason } = tool_policy.evaluate(target.tool_name) {
            warn!(
                dispatch_id = %target.dispatch_id,
                tool_name = %target.tool_name,
                deny_reason = reason,
                "dispatch denied by tool policy"
            );
            return Err(GateDenial {
                last_dispatch_outcome: "policy_denied",
                reason: format!(
                    "tool '{}' denied by dispatch policy ({})",
                    target.tool_name, reason
                ),
            });
        }
    }

    if policy.env_denylist.is_some() || policy.env_allowlist.is_some() {
        // #1601: the effective env surface is the union of the keys the
        // dispatch payload tries to forward (`task["env"]`, e.g. the
        // supervised CLI specialist path) and the keys the backend
        // itself is configured to set on the spawned process.
        // Payload-only inspection had no live input — no MCP dispatch
        // site sets `env` on the payload.
        let env_keys: Vec<&str> = target
            .task
            .get("env")
            .and_then(|env| env.as_object())
            .into_iter()
            .flat_map(|env| env.keys().map(String::as_str))
            .chain(backend.env_keys().iter().map(String::as_str))
            .collect();

        if let Some(ref denylist) = policy.env_denylist {
            if let Some(forbidden) = first_denied_env_key(&env_keys, denylist) {
                warn!(
                    dispatch_id = %target.dispatch_id,
                    forbidden_key = %forbidden,
                    "dispatch denied by env denylist"
                );
                return Err(GateDenial {
                    last_dispatch_outcome: "env_forbidden",
                    reason: format!(
                        "env variable '{forbidden}' is denied by the dispatch denylist (injection-class env vars are blocked)"
                    ),
                });
            }
        }

        if let Some(ref allowlist) = policy.env_allowlist {
            if let Some(forbidden) = first_forbidden_env_key(&env_keys, allowlist) {
                warn!(
                    dispatch_id = %target.dispatch_id,
                    forbidden_key = %forbidden,
                    "dispatch denied by env allowlist"
                );
                return Err(GateDenial {
                    last_dispatch_outcome: "env_forbidden",
                    reason: format!("env variable '{forbidden}' is not in the dispatch allowlist"),
                });
            }
        }
    }

    if policy.require_approval {
        let Some(ref requester) = policy.approval_requester else {
            warn!(
                dispatch_id = %target.dispatch_id,
                "dispatch requires approval but no approver is wired"
            );
            return Err(GateDenial {
                last_dispatch_outcome: "approval_unavailable",
                reason: format!(
                    "dispatch policy requires approval but no requester is wired (dispatch '{}')",
                    target.dispatch_id
                ),
            });
        };
        let request = ToolApprovalRequest {
            tool_id: target.dispatch_id.to_string(),
            tool_name: target.tool_name.to_string(),
            title: format!("Approve dispatch for {}", target.tool_name),
            body: format!(
                "Backend '{}' (endpoint '{}') will receive dispatch '{}'.",
                backend.backend_label(),
                backend.endpoint_label(),
                target.dispatch_id
            ),
            command: None,
            cwd: None,
        };
        let decision = requester.request_approval(request).await;
        if matches!(decision, ToolApprovalDecision::Deny) {
            warn!(
                dispatch_id = %target.dispatch_id,
                tool_name = %target.tool_name,
                "dispatch denied by approval requester"
            );
            return Err(GateDenial {
                last_dispatch_outcome: "approval_denied",
                reason: format!(
                    "dispatch for tool '{}' denied by approval requester",
                    target.tool_name
                ),
            });
        }
    }

    Ok(())
}

fn first_forbidden_env_key(env_keys: &[&str], allowlist: &HashSet<String>) -> Option<String> {
    for key in env_keys {
        let normalized = key.to_ascii_uppercase();
        if !allowlist.contains(&normalized) {
            return Some((*key).to_string());
        }
    }
    None
}

fn first_denied_env_key(env_keys: &[&str], denylist: &HashSet<String>) -> Option<String> {
    for key in env_keys {
        let normalized = key.to_ascii_uppercase();
        if denylist.contains(&normalized) {
            return Some((*key).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    use crate::tools::mcp_agent::{
        CliAgentBackend, DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackendConfig,
        StdioMcpAgent,
    };

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

    fn target<'a>(id: &'a str, tool: &'a str, task: &'a serde_json::Value) -> DispatchTarget<'a> {
        DispatchTarget {
            dispatch_id: id,
            tool_name: tool,
            task,
        }
    }

    #[tokio::test]
    async fn noop_policy_passes_every_dispatch() {
        let policy = DispatchPolicy::default();
        let backend = StubBackend;
        let task = json!({});
        assert!(
            enforce_dispatch_gates(&policy, &backend, target("c1", "any_tool", &task))
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
        let task = json!({});
        let denial = enforce_dispatch_gates(&policy, &backend, target("c1", "forbidden", &task))
            .await
            .expect_err("denied");
        assert_eq!(denial.last_dispatch_outcome, "policy_denied");
        assert!(denial.reason.contains("forbidden"));
    }

    #[tokio::test]
    async fn approval_required_without_requester_fails_closed() {
        let policy = DispatchPolicy {
            require_approval: true,
            ..Default::default()
        };
        let backend = StubBackend;
        let task = json!({});
        let denial = enforce_dispatch_gates(&policy, &backend, target("c1", "any_tool", &task))
            .await
            .expect_err("denied");
        assert_eq!(denial.last_dispatch_outcome, "approval_unavailable");
    }

    #[tokio::test]
    async fn env_denylist_blocks_known_injection_keys() {
        let mut denylist = HashSet::new();
        denylist.insert("LD_PRELOAD".to_string());

        let policy = DispatchPolicy {
            env_denylist: Some(denylist),
            ..Default::default()
        };
        let backend = StubBackend;
        let task = json!({"env": {"LD_PRELOAD": "/tmp/evil.so"}});
        let denial = enforce_dispatch_gates(&policy, &backend, target("c1", "any", &task))
            .await
            .expect_err("denied");
        assert_eq!(denial.last_dispatch_outcome, "env_forbidden");
        assert!(denial.reason.contains("LD_PRELOAD"));
    }

    /// #1601: no dispatch payload ever carries an `env` object — the
    /// env that actually reaches a spawned sub-agent comes from the
    /// backend's own configuration. The gate must inspect those
    /// configured keys too, or the denylist has no live input.
    #[tokio::test]
    async fn env_denylist_blocks_backend_configured_env_keys() {
        let policy = DispatchPolicy::from_agent_gates(None, true);
        let backend = StdioMcpAgent::from_config(&McpAgentBackendConfig::Local {
            cmd: "echo".to_string(),
            args: Vec::new(),
            env: std::collections::HashMap::from([(
                "LD_PRELOAD".to_string(),
                "/tmp/evil.so".to_string(),
            )]),
            dispatch_timeout_secs: None,
        })
        .expect("valid backend config");
        let task = json!({"task": "noop"});
        let denial = enforce_dispatch_gates(&policy, &backend, target("c1", "any", &task))
            .await
            .expect_err("backend-configured LD_PRELOAD must fail closed");
        assert_eq!(denial.last_dispatch_outcome, "env_forbidden");
        assert!(denial.reason.contains("LD_PRELOAD"));
    }

    /// #1601: backend-configured keys are matched case-insensitively,
    /// same as payload keys.
    #[tokio::test]
    async fn env_denylist_blocks_backend_env_keys_case_insensitively() {
        let policy = DispatchPolicy::from_agent_gates(None, true);
        let backend = StdioMcpAgent::from_config(&McpAgentBackendConfig::Local {
            cmd: "echo".to_string(),
            args: Vec::new(),
            env: std::collections::HashMap::from([(
                "ld_preload".to_string(),
                "/tmp/evil.so".to_string(),
            )]),
            dispatch_timeout_secs: None,
        })
        .expect("valid backend config");
        let task = json!({"task": "noop"});
        let denial = enforce_dispatch_gates(&policy, &backend, target("c1", "any", &task))
            .await
            .expect_err("lowercase ld_preload must fail closed");
        assert_eq!(denial.last_dispatch_outcome, "env_forbidden");
        assert!(denial.reason.contains("ld_preload"));
    }

    /// #1601: the allowlist gate inspects backend-configured keys too.
    #[tokio::test]
    async fn env_allowlist_blocks_backend_env_key_not_listed() {
        let mut allowlist = HashSet::new();
        allowlist.insert("PATH".to_string());

        let policy = DispatchPolicy {
            env_allowlist: Some(allowlist),
            ..Default::default()
        };
        let backend = StdioMcpAgent::from_config(&McpAgentBackendConfig::Local {
            cmd: "echo".to_string(),
            args: Vec::new(),
            env: std::collections::HashMap::from([(
                "ACME_TOKEN".to_string(),
                "secret".to_string(),
            )]),
            dispatch_timeout_secs: None,
        })
        .expect("valid backend config");
        let task = json!({"task": "noop"});
        let denial = enforce_dispatch_gates(&policy, &backend, target("c1", "any", &task))
            .await
            .expect_err("backend env key outside the allowlist must be denied");
        assert_eq!(denial.last_dispatch_outcome, "env_forbidden");
        assert!(denial.reason.contains("ACME_TOKEN"));
    }

    /// #1601: harmless backend env keys must not trip the injection
    /// denylist — the gate only fails closed on listed names.
    #[tokio::test]
    async fn env_denylist_permits_benign_backend_env_keys() {
        let policy = DispatchPolicy::from_agent_gates(None, true);
        let backend = StdioMcpAgent::from_config(&McpAgentBackendConfig::Local {
            cmd: "echo".to_string(),
            args: Vec::new(),
            env: std::collections::HashMap::from([(
                "ACME_REGION".to_string(),
                "us-test-1".to_string(),
            )]),
            dispatch_timeout_secs: None,
        })
        .expect("valid backend config");
        let task = json!({"task": "noop"});
        assert!(
            enforce_dispatch_gates(&policy, &backend, target("c1", "any", &task))
                .await
                .is_ok()
        );
    }

    /// #1601: payload `env` and backend-configured env are inspected as
    /// one union — a benign payload must not dilute a denied
    /// backend-configured key, and vice versa.
    #[tokio::test]
    async fn env_denylist_unions_payload_and_backend_env_keys() {
        let policy = DispatchPolicy::from_agent_gates(None, true);
        let backend = StdioMcpAgent::from_config(&McpAgentBackendConfig::Local {
            cmd: "echo".to_string(),
            args: Vec::new(),
            env: std::collections::HashMap::from([(
                "NODE_OPTIONS".to_string(),
                "--require /tmp/evil.js".to_string(),
            )]),
            dispatch_timeout_secs: None,
        })
        .expect("valid backend config");
        let task = json!({"env": {"ACME_REGION": "us-test-1"}});
        let denial = enforce_dispatch_gates(&policy, &backend, target("c1", "any", &task))
            .await
            .expect_err(
                "backend-configured NODE_OPTIONS must fail closed even with a benign payload env",
            );
        assert_eq!(denial.last_dispatch_outcome, "env_forbidden");
        assert!(denial.reason.contains("NODE_OPTIONS"));
    }

    /// #1601: `from_mcp_backend` captures the CLI backend's configured
    /// env keys so `enforce_dispatch_gates_for_backend` callers (the
    /// supervised MCP specialist path) get the same live gate input.
    #[tokio::test]
    async fn cli_backend_env_keys_flow_into_backend_metadata() {
        let policy = DispatchPolicy::from_agent_gates(None, true);
        let backend = CliAgentBackend::from_config(&McpAgentBackendConfig::Cli {
            cmd: "echo".to_string(),
            args: Vec::new(),
            env: std::collections::HashMap::from([(
                "DYLD_INSERT_LIBRARIES".to_string(),
                "/tmp/evil.dylib".to_string(),
            )]),
            dispatch_timeout_secs: None,
            prompt_via_stdin: false,
        })
        .expect("valid backend config");
        let metadata = DispatchBackendMetadata::from_mcp_backend(&backend);
        assert!(
            metadata
                .env_keys()
                .iter()
                .any(|key| key == "DYLD_INSERT_LIBRARIES")
        );
        let task = json!({"task": "noop"});
        let denial =
            enforce_dispatch_gates_for_backend(&policy, &metadata, target("c1", "any", &task))
                .await
                .expect_err("CLI backend configured with DYLD_INSERT_LIBRARIES must fail closed");
        assert_eq!(denial.last_dispatch_outcome, "env_forbidden");
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

    /// #713/#714: with no operator tool policy and the env denylist on,
    /// the policy is **not** a no-op — it still gates against
    /// injection env vars.
    #[tokio::test]
    async fn from_agent_gates_with_only_denylist_is_not_noop() {
        let policy = DispatchPolicy::from_agent_gates(None, true);
        assert!(
            !policy.is_noop(),
            "a denylist-only policy must run the gate"
        );
    }
}
