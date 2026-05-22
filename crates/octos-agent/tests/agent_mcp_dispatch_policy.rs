//! Regression test for #714 — the `agent_mcp` spawn variant must
//! honour [`DispatchPolicy`] gates the same way the native swarm
//! dispatcher does.
//!
//! Pre-fix, [`SpawnTool::execute`] dispatched via
//! [`octos_agent::tools::mcp_agent::dispatch_with_metrics`] without
//! consulting any policy, so rate-limit / fan-out / denylist
//! constraints were trivially bypassed by a malicious or buggy MCP
//! server. Companion gap to the supervisor fan-out cap shipped via
//! #607 / #610 — the agent_mcp path slipped through.
//!
//! The test models the "fan-out cap" the bug report calls out by
//! wiring a stateful approval requester that approves the first N
//! dispatches and denies the rest. Without the fix, the spawn site
//! never consults the requester so every dispatch succeeds; with the
//! fix, the (N+1)-th dispatch is rejected by the policy gate before
//! the backend is touched.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use octos_agent::tools::mcp_agent::{
    DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackend,
};
use octos_agent::tools::{DispatchPolicy, SharedBackend, SpawnTool, Tool};
use octos_agent::{ToolApprovalDecision, ToolApprovalRequest, ToolApprovalRequester};
use octos_core::InboundMessage;
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

/// MCP backend that always reports success. Counts the dispatches it
/// served so the test can assert the policy gate fired before this
/// backend was touched.
struct CountingBackend {
    served: AtomicU32,
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
        self.served.fetch_add(1, Ordering::SeqCst);
        DispatchResponse {
            outcome: DispatchOutcome::Success,
            output: "ok".to_string(),
            files_to_send: Vec::new(),
            error: None,
            context_contract: None,
        }
    }
}

/// Stateful approval requester that approves the first `cap` requests
/// and denies the rest. Models the "fan-out cap" semantics the bug
/// report describes — the underlying constraint enforced via
/// [`DispatchPolicy::require_approval`] + a stateful requester.
struct CappedApprover {
    cap: u32,
    seen: AtomicU32,
}

#[async_trait]
impl ToolApprovalRequester for CappedApprover {
    async fn request_approval(&self, _: ToolApprovalRequest) -> ToolApprovalDecision {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        if n < self.cap {
            ToolApprovalDecision::Approve
        } else {
            ToolApprovalDecision::Deny
        }
    }
}

struct NullLlm;

#[async_trait]
impl LlmProvider for NullLlm {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        Ok(ChatResponse {
            content: Some("ok".to_string()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "null"
    }

    fn provider_name(&self) -> &str {
        "null"
    }
}

async fn memory(dir: &TempDir) -> Arc<EpisodeStore> {
    Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap())
}

#[tokio::test]
async fn agent_mcp_spawn_respects_dispatch_policy_fanout_cap_per_714() {
    let dir = TempDir::new().unwrap();
    let memory = memory(&dir).await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(8);

    let backend_inner = Arc::new(CountingBackend {
        served: AtomicU32::new(0),
    });
    let backend: SharedBackend = backend_inner.clone();

    let approver = Arc::new(CappedApprover {
        cap: 2,
        seen: AtomicU32::new(0),
    });
    let policy = DispatchPolicy {
        require_approval: true,
        approval_requester: Some(approver.clone() as Arc<dyn ToolApprovalRequester>),
        ..Default::default()
    };

    let llm: Arc<dyn LlmProvider> = Arc::new(NullLlm);
    let spawn = SpawnTool::new(llm, memory, PathBuf::from(dir.path()), tx)
        .with_mcp_agent_backend(backend, Some("run_task".to_string()))
        .with_dispatch_policy(policy);

    // Dispatch three sub-agents through the agent_mcp path. The first
    // two MUST be approved by the policy; the third MUST be rejected
    // before the backend is touched.
    let r1 = spawn
        .execute(&serde_json::json!({
            "task": "first",
            "mode": "sync",
            "backend": "agent_mcp",
            "label": "fanout-1",
        }))
        .await
        .unwrap();
    let r2 = spawn
        .execute(&serde_json::json!({
            "task": "second",
            "mode": "sync",
            "backend": "agent_mcp",
            "label": "fanout-2",
        }))
        .await
        .unwrap();
    let r3 = spawn
        .execute(&serde_json::json!({
            "task": "third",
            "mode": "sync",
            "backend": "agent_mcp",
            "label": "fanout-3",
        }))
        .await
        .unwrap();

    assert!(r1.success, "first dispatch under cap must succeed");
    assert!(r2.success, "second dispatch at cap must succeed");
    assert!(
        !r3.success,
        "third dispatch over fan-out cap must be rejected by DispatchPolicy; got success=true, output=`{}`",
        r3.output
    );
    assert!(
        r3.output.contains("approval") || r3.output.contains("denied"),
        "rejection output must mention the policy denial; got `{}`",
        r3.output
    );

    // The backend MUST only have served the two approved dispatches;
    // if the policy gate is skipped, this count would be 3.
    let served = backend_inner.served.load(Ordering::SeqCst);
    assert_eq!(
        served, 2,
        "backend served {served} dispatches; expected 2 (third must be blocked by DispatchPolicy before backend is touched)"
    );

    // The approver MUST have been consulted at least three times — the
    // gate must fail closed, not silently bypass when no requester is
    // available.
    let consults = approver.seen.load(Ordering::SeqCst);
    assert_eq!(
        consults, 3,
        "approver consulted {consults} times; expected 3 (one per spawn)"
    );
}

/// #714 follow-up (codex review): the public `dispatch_to_mcp_agent`
/// helper is a thin wrapper around `dispatch_with_metrics`. A
/// configured [`DispatchPolicy`] must gate this entry point too —
/// callers that route through the helper cannot be allowed to bypass
/// the gate the main spawn execute path enforces.
#[tokio::test]
async fn dispatch_to_mcp_agent_helper_respects_dispatch_policy_per_714() {
    let dir = TempDir::new().unwrap();
    let memory = memory(&dir).await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(8);

    let backend_inner = Arc::new(CountingBackend {
        served: AtomicU32::new(0),
    });
    let backend: SharedBackend = backend_inner.clone();

    // Tool policy that denies the default MCP dispatch tool name. The
    // gate must reject the helper invocation before the backend is
    // touched.
    let policy = DispatchPolicy {
        tool_policy: Some(octos_agent::ToolPolicy {
            deny: vec!["run_task".into()],
            ..Default::default()
        }),
        ..Default::default()
    };

    let llm: Arc<dyn LlmProvider> = Arc::new(NullLlm);
    let spawn = SpawnTool::new(llm, memory, PathBuf::from(dir.path()), tx)
        .with_mcp_agent_backend(backend, Some("run_task".to_string()))
        .with_dispatch_policy(policy);

    let (response, _event) = spawn
        .dispatch_to_mcp_agent(
            serde_json::json!({"task": "helper bypass attempt"}),
            "session-helper",
            "task-helper",
            None,
            None,
        )
        .await
        .expect("helper returns a synthesized response on denial, not an error");

    assert_eq!(
        response.outcome,
        DispatchOutcome::RemoteError,
        "policy-denied helper dispatch must surface RemoteError, not Success"
    );
    let error = response.error.expect("denied response carries error");
    assert!(
        error.contains("policy") || error.contains("denied"),
        "error must mention the policy gate; got `{error}`"
    );
    assert_eq!(
        backend_inner.served.load(Ordering::SeqCst),
        0,
        "backend must NOT be touched when the policy gate denies the helper dispatch"
    );
}
