//! Integration tests for the M7 req 7 dispatch policy gate.
//!
//! These tests stand in for the audit's `mcp_backend_respects_*` and
//! `cli_backend_respects_*` cases. The swarm crate exposes a single
//! [`octos_swarm::McpAgentBackend`] trait that every backend funnels
//! through (stdio, HTTP, native sub-agent via SpawnTool), so a fake
//! backend covers all three execution paths from the gate's
//! perspective. Each test asserts:
//!
//! 1. The fake backend is **not** invoked when the gate denies
//!    (`dispatch_count == 0`).
//! 2. The synthesised [`octos_swarm::SubtaskOutcome`] carries a stable
//!    `last_dispatch_outcome` label so the harness observability
//!    channel renders the denial uniformly across topologies.
//! 3. With a default (no-op) policy the existing tests' behaviour is
//!    preserved (regression).

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use octos_agent::tools::mcp_agent::{
    DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackend,
};
use octos_agent::{ToolApprovalDecision, ToolApprovalRequest, ToolApprovalRequester, ToolPolicy};
use octos_swarm::{
    ContractSpec, DispatchPolicy, Swarm, SwarmBudget, SwarmContext, SwarmOutcomeKind, SwarmTopology,
};

// ── Helpers ────────────────────────────────────────────────────────────────

/// Backend that counts dispatch calls and always succeeds. Used to
/// prove the gate short-circuits before the backend is invoked.
#[derive(Default)]
struct CountingBackend {
    counter: AtomicUsize,
}

impl CountingBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn count(&self) -> usize {
        self.counter.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl McpAgentBackend for CountingBackend {
    fn backend_label(&self) -> &'static str {
        "local"
    }
    fn endpoint_label(&self) -> String {
        "counting".to_string()
    }
    async fn dispatch(&self, _request: DispatchRequest) -> DispatchResponse {
        self.counter.fetch_add(1, Ordering::SeqCst);
        DispatchResponse {
            outcome: DispatchOutcome::Success,
            output: "ok".to_string(),
            files_to_send: Vec::new(),
            error: None,
        }
    }
}

/// HTTP-flavoured counting backend so we can prove the gate fires
/// uniformly regardless of `backend_label()`. The audit treats `"local"`
/// (stdio MCP) and `"remote"` (HTTP MCP) as separate execution paths,
/// even though they share the same `McpAgentBackend` trait.
#[derive(Default)]
struct HttpCountingBackend {
    counter: AtomicUsize,
}

impl HttpCountingBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn count(&self) -> usize {
        self.counter.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl McpAgentBackend for HttpCountingBackend {
    fn backend_label(&self) -> &'static str {
        "remote"
    }
    fn endpoint_label(&self) -> String {
        "https://example.com/mcp".to_string()
    }
    async fn dispatch(&self, _request: DispatchRequest) -> DispatchResponse {
        self.counter.fetch_add(1, Ordering::SeqCst);
        DispatchResponse {
            outcome: DispatchOutcome::Success,
            output: "ok".to_string(),
            files_to_send: Vec::new(),
            error: None,
        }
    }
}

fn ctx() -> SwarmContext {
    SwarmContext {
        session_id: "api:test-policy".into(),
        task_id: "task-policy".into(),
        workflow: Some("swarm_policy_test".into()),
        phase: Some("dispatch".into()),
    }
}

fn contract(id: &str, tool: &str) -> ContractSpec {
    ContractSpec {
        contract_id: id.into(),
        tool_name: tool.into(),
        task: serde_json::json!({"contract_id": id}),
        label: Some(format!("c-{id}")),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// AUDIT MAPPING: `mcp_backend_respects_tool_policy_deny` —
/// MCP (local) backend dispatch must NOT execute when the tool policy
/// denies the contract's tool name. The synthesised outcome uses the
/// `policy_denied` label so the harness sees the denial uniformly.
#[tokio::test]
async fn local_mcp_backend_respects_tool_policy_deny() {
    let backend = CountingBackend::new();
    let dir = tempfile::tempdir().unwrap();

    let policy = DispatchPolicy {
        tool_policy: Some(ToolPolicy {
            deny: vec!["forbidden_tool".into()],
            ..Default::default()
        }),
        ..Default::default()
    };

    let swarm = Swarm::builder(backend.clone(), dir.path())
        .with_dispatch_policy(policy)
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "policy-deny",
            vec![contract("c1", "forbidden_tool")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            SwarmBudget::default(),
            ctx(),
        )
        .await
        .unwrap();

    assert_eq!(backend.count(), 0, "backend must NOT dispatch when denied");
    assert_eq!(result.outcome, SwarmOutcomeKind::Failed);
    let outcome = &result.per_task_outcomes[0];
    assert_eq!(outcome.last_dispatch_outcome, "policy_denied");
    assert!(outcome.error.as_deref().unwrap().contains("forbidden_tool"));
}

/// AUDIT MAPPING: equivalent of `cli_backend_respects_tool_policy_deny`
/// — the same gate guards the remote/HTTP backend (which the swarm
/// reaches via the same `McpAgentBackend` trait). The audit's "CLI
/// backend" maps onto the `McpAgentBackend` trait because there is no
/// separate CLI backend in the swarm crate today (the audit clarifies
/// this in req 1's evidence: "The swarm dispatcher only knows about
/// MCP backends").
#[tokio::test]
async fn remote_mcp_backend_respects_tool_policy_deny() {
    let backend = HttpCountingBackend::new();
    let dir = tempfile::tempdir().unwrap();

    let policy = DispatchPolicy {
        tool_policy: Some(ToolPolicy {
            allow: vec!["only_this_tool".into()],
            ..Default::default()
        }),
        ..Default::default()
    };

    let swarm = Swarm::builder(backend.clone(), dir.path())
        .with_dispatch_policy(policy)
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "remote-policy-deny",
            vec![contract("c1", "different_tool")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            SwarmBudget::default(),
            ctx(),
        )
        .await
        .unwrap();

    assert_eq!(backend.count(), 0);
    assert_eq!(result.outcome, SwarmOutcomeKind::Failed);
    assert_eq!(
        result.per_task_outcomes[0].last_dispatch_outcome,
        "policy_denied"
    );
}

/// AUDIT MAPPING: `cli_backend_respects_approval_gate` —
/// approval is required, no requester is wired -> dispatch must fail
/// closed. The synthesised outcome uses `approval_unavailable` so
/// operators can distinguish "no approver" from a user-issued deny.
#[tokio::test]
async fn approval_required_without_requester_fails_closed() {
    let backend = CountingBackend::new();
    let dir = tempfile::tempdir().unwrap();

    let policy = DispatchPolicy {
        require_approval: true,
        approval_requester: None,
        ..Default::default()
    };

    let swarm = Swarm::builder(backend.clone(), dir.path())
        .with_dispatch_policy(policy)
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "approval-missing",
            vec![contract("c1", "any_tool")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            SwarmBudget::default(),
            ctx(),
        )
        .await
        .unwrap();

    assert_eq!(backend.count(), 0);
    assert_eq!(result.outcome, SwarmOutcomeKind::Failed);
    assert_eq!(
        result.per_task_outcomes[0].last_dispatch_outcome,
        "approval_unavailable"
    );
}

/// AUDIT MAPPING: extension of `cli_backend_respects_approval_gate` —
/// approval requester returns Deny -> backend NOT dispatched.
#[tokio::test]
async fn approval_deny_blocks_dispatch() {
    struct DenyRequester {
        seen: Mutex<usize>,
    }

    #[async_trait]
    impl ToolApprovalRequester for DenyRequester {
        async fn request_approval(&self, _: ToolApprovalRequest) -> ToolApprovalDecision {
            *self.seen.lock().unwrap() += 1;
            ToolApprovalDecision::Deny
        }
    }

    let requester = Arc::new(DenyRequester {
        seen: Mutex::new(0),
    });
    let backend = CountingBackend::new();
    let dir = tempfile::tempdir().unwrap();

    let policy = DispatchPolicy {
        require_approval: true,
        approval_requester: Some(requester.clone()),
        ..Default::default()
    };

    let swarm = Swarm::builder(backend.clone(), dir.path())
        .with_dispatch_policy(policy)
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "approval-deny",
            vec![contract("c1", "shell")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            SwarmBudget::default(),
            ctx(),
        )
        .await
        .unwrap();

    assert_eq!(*requester.seen.lock().unwrap(), 1);
    assert_eq!(backend.count(), 0);
    assert_eq!(
        result.per_task_outcomes[0].last_dispatch_outcome,
        "approval_denied"
    );
}

/// AUDIT MAPPING: counterpart to the deny test — `Approve` lets the
/// dispatch through and the backend is invoked exactly once.
#[tokio::test]
async fn approval_approve_lets_dispatch_through() {
    struct ApproveRequester;

    #[async_trait]
    impl ToolApprovalRequester for ApproveRequester {
        async fn request_approval(&self, _: ToolApprovalRequest) -> ToolApprovalDecision {
            ToolApprovalDecision::Approve
        }
    }

    let backend = CountingBackend::new();
    let dir = tempfile::tempdir().unwrap();

    let policy = DispatchPolicy {
        require_approval: true,
        approval_requester: Some(Arc::new(ApproveRequester)),
        ..Default::default()
    };

    let swarm = Swarm::builder(backend.clone(), dir.path())
        .with_dispatch_policy(policy)
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "approval-pass",
            vec![contract("c1", "shell")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            SwarmBudget::default(),
            ctx(),
        )
        .await
        .unwrap();

    assert_eq!(backend.count(), 1);
    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
}

/// AUDIT MAPPING: `mcp_backend_respects_env_allowlist` — the gate
/// inspects the contract's task payload for an `env` object whose keys
/// are tested against the configured allowlist. Forbidden keys cause
/// `env_forbidden` denial **before** the backend is touched.
#[tokio::test]
async fn env_allowlist_blocks_forbidden_keys() {
    let backend = CountingBackend::new();
    let dir = tempfile::tempdir().unwrap();

    let mut allowlist = HashSet::new();
    allowlist.insert("OPENAI_API_KEY".to_string());

    let policy = DispatchPolicy {
        env_allowlist: Some(allowlist),
        ..Default::default()
    };

    let swarm = Swarm::builder(backend.clone(), dir.path())
        .with_dispatch_policy(policy)
        .build()
        .await
        .unwrap();

    // Task carries a forbidden env key (LD_PRELOAD style injection).
    let bad_contract = ContractSpec {
        contract_id: "c1".into(),
        tool_name: "any".into(),
        task: serde_json::json!({
            "contract_id": "c1",
            "env": {"LD_PRELOAD": "/tmp/evil.so"},
        }),
        label: None,
    };

    let result = swarm
        .dispatch(
            "env-deny",
            vec![bad_contract],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            SwarmBudget::default(),
            ctx(),
        )
        .await
        .unwrap();

    assert_eq!(backend.count(), 0);
    assert_eq!(
        result.per_task_outcomes[0].last_dispatch_outcome,
        "env_forbidden"
    );
    assert!(
        result.per_task_outcomes[0]
            .error
            .as_deref()
            .unwrap()
            .contains("LD_PRELOAD")
    );
}

/// `require_sandboxed: true` against an unsandboxed backend must fail
/// closed. Today no `McpAgentBackend` self-reports as sandboxed, so the
/// gate denies every dispatch when this flag is set — that's the
/// conservative choice matching M7 req 7's safety bar (the audit notes
/// the HTTP backend's remote sandbox is opaque to the parent).
#[tokio::test]
async fn require_sandboxed_blocks_unsandboxed_backend() {
    let backend = CountingBackend::new();
    let dir = tempfile::tempdir().unwrap();

    let policy = DispatchPolicy {
        require_sandboxed: true,
        ..Default::default()
    };

    let swarm = Swarm::builder(backend.clone(), dir.path())
        .with_dispatch_policy(policy)
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "sandbox-required",
            vec![contract("c1", "shell")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            SwarmBudget::default(),
            ctx(),
        )
        .await
        .unwrap();

    assert_eq!(backend.count(), 0);
    assert_eq!(
        result.per_task_outcomes[0].last_dispatch_outcome,
        "sandbox_required"
    );
}

/// AUDIT MAPPING: `native_backend_still_works_with_dispatch_gate_lifted`
/// — regression: with no policy configured (default builder), the
/// existing behaviour is unchanged. The backend dispatches normally,
/// the result is `Success`, and the synthesised outcome carries the
/// regular `success` label.
#[tokio::test]
async fn no_policy_preserves_legacy_behaviour() {
    let backend = CountingBackend::new();
    let dir = tempfile::tempdir().unwrap();

    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "legacy",
            vec![
                contract("c1", "tool_a"),
                contract("c2", "tool_b"),
                contract("c3", "tool_c"),
            ],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(3).unwrap(),
            },
            SwarmBudget::default(),
            ctx(),
        )
        .await
        .unwrap();

    assert_eq!(backend.count(), 3);
    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
    for outcome in &result.per_task_outcomes {
        assert_eq!(outcome.last_dispatch_outcome, "success");
    }
}

/// Sequential topology must abort on a gate denial just as it aborts on
/// a backend `TerminalFailed`. This protects pipelines from poisoning
/// downstream stages with a `policy_denied` upstream output.
#[tokio::test]
async fn sequential_aborts_on_gate_denial() {
    let backend = CountingBackend::new();
    let dir = tempfile::tempdir().unwrap();

    let policy = DispatchPolicy {
        tool_policy: Some(ToolPolicy {
            deny: vec!["forbidden".into()],
            ..Default::default()
        }),
        ..Default::default()
    };

    let swarm = Swarm::builder(backend.clone(), dir.path())
        .with_dispatch_policy(policy)
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "seq-deny",
            vec![
                contract("c1", "ok_tool"),
                contract("c2", "forbidden"),
                contract("c3", "ok_tool"),
            ],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            ctx(),
        )
        .await
        .unwrap();

    // c1 dispatched (success), c2 denied (TerminalFailed -> sequential
    // aborts), c3 never ran.
    assert_eq!(backend.count(), 1);
    assert_eq!(result.outcome, SwarmOutcomeKind::Aborted);
    assert_eq!(
        result.per_task_outcomes[1].last_dispatch_outcome,
        "policy_denied"
    );
    assert_eq!(result.per_task_outcomes[2].last_dispatch_outcome, "not_run");
}
