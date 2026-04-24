//! Acceptance tests for M6.7 — synchronous DelegateTool with MAX_DEPTH guard.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use octos_agent::harness_errors::HarnessError;
use octos_agent::task_supervisor::{TaskLifecycleState, TaskStatus, TaskSupervisor};
use octos_agent::tools::{
    DELEGATED_DENY_GROUP, DelegateTool, DepthBudget, MAX_DEPTH, Tool, ToolPolicy,
    build_delegated_child_policy,
};
use octos_core::Message;
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

/// Minimal LLM provider — always returns a scripted natural-language reply
/// with `EndTurn`. The delegate child drives this directly through
/// `Agent::run_task`, so one scripted reply per run is enough.
struct EchoProvider {
    reply: String,
}

#[async_trait]
impl LlmProvider for EchoProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        Ok(ChatResponse {
            content: Some(self.reply.clone()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "echo-mock"
    }

    fn provider_name(&self) -> &str {
        "echo-mock"
    }
}

async fn memory(dir: &TempDir) -> Arc<EpisodeStore> {
    Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap())
}

fn llm(reply: &str) -> Arc<dyn LlmProvider> {
    Arc::new(EchoProvider {
        reply: reply.to_string(),
    })
}

#[tokio::test]
async fn should_block_parent_until_child_terminal() {
    // Invariant #5: parent blocks until the child lifecycle state is
    // Ready/Failed. We observe this by asserting that by the time
    // `execute` returns, the supervisor's task is in a terminal state.
    let dir = TempDir::new().unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    let memory = memory(&dir).await;

    let tool = DelegateTool::new(llm("done"), memory, PathBuf::from(dir.path()))
        .with_task_supervisor(supervisor.clone(), "api:test-session");

    let result = tool
        .execute(&serde_json::json!({
            "task": "produce a short report",
            "label": "subtask"
        }))
        .await
        .unwrap();

    assert!(result.success, "child should succeed: {}", result.output);
    assert_eq!(result.output, "done");

    let tasks = supervisor.get_tasks_for_session("api:test-session");
    assert_eq!(tasks.len(), 1);
    let child = &tasks[0];
    assert_eq!(child.tool_name, "subtask");
    // By the time `execute` returned, the child must be in a terminal
    // lifecycle state (Ready or Failed).
    assert!(
        matches!(
            child.lifecycle_state(),
            TaskLifecycleState::Ready | TaskLifecycleState::Failed
        ),
        "child lifecycle state must be terminal, got {:?}",
        child.lifecycle_state()
    );
    assert_eq!(child.status, TaskStatus::Completed);
}

#[tokio::test]
async fn should_deny_grandchild_delegation_with_typed_error() {
    // Invariant #1: a parent already at MAX_DEPTH must reject delegation
    // synchronously with `HarnessError::DelegateDepthExceeded`.
    let dir = TempDir::new().unwrap();
    let memory = memory(&dir).await;

    let tool = DelegateTool::new(llm("irrelevant"), memory, PathBuf::from(dir.path()))
        .with_depth_budget(DepthBudget::at_level(MAX_DEPTH));

    let result = tool
        .execute(&serde_json::json!({ "task": "should never run" }))
        .await;
    let error = match result {
        Ok(_) => panic!("depth budget exhausted must fail synchronously"),
        Err(error) => error,
    };

    let harness = error
        .downcast_ref::<HarnessError>()
        .expect("error must downcast to HarnessError");
    match harness {
        HarnessError::DelegateDepthExceeded {
            depth,
            limit,
            message,
        } => {
            assert_eq!(*depth, MAX_DEPTH);
            assert_eq!(*limit, MAX_DEPTH);
            assert!(message.contains(&format!("depth {MAX_DEPTH}")));
        }
        other => panic!("expected DelegateDepthExceeded, got {other:?}"),
    }
}

#[tokio::test]
async fn should_apply_group_delegated_deny_list_to_child() {
    // Invariant #3: the child's policy must deny every tool named in
    // `group:delegated` (via deny-wins semantics). We drive it directly
    // through the policy factory so the test remains hermetic.
    let policy = build_delegated_child_policy(vec!["read_file".into(), "shell".into()]);

    assert!(policy.deny.contains(&DELEGATED_DENY_GROUP.to_string()));
    assert!(!policy.is_allowed("delegate_task"));
    assert!(!policy.is_allowed("spawn"));
    assert!(!policy.is_allowed("send_message"));
    assert!(!policy.is_allowed("message"));
    assert!(!policy.is_allowed("save_memory"));
    assert!(!policy.is_allowed("execute_code"));
    // Explicitly allowed non-denied tools remain usable by the child.
    assert!(policy.is_allowed("read_file"));
    assert!(policy.is_allowed("shell"));
}

#[tokio::test]
async fn should_deliver_child_artifact_through_contract_gate() {
    // Invariant #4: child returns via the contract-gated delivery. When
    // the working directory contains a declared-but-unready workspace
    // contract, DelegateTool must surface the contract failure rather
    // than reporting a bare success.
    let dir = TempDir::new().unwrap();
    let repo_root = dir.path().join("slides/demo");
    std::fs::create_dir_all(&repo_root).unwrap();
    octos_agent::write_workspace_policy(
        &repo_root,
        &octos_agent::WorkspacePolicy::for_kind(octos_agent::WorkspaceProjectKind::Slides),
    )
    .unwrap();
    // Declared policy is present, but required deliverables are missing —
    // the contract should be reported as unready.
    std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
    std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();

    let supervisor = Arc::new(TaskSupervisor::new());
    let memory = memory(&dir).await;

    let tool = DelegateTool::new(llm("done"), memory, PathBuf::from(dir.path()))
        .with_task_supervisor(supervisor.clone(), "api:test-session");

    let result = tool
        .execute(&serde_json::json!({
            "task": "attempt to complete the slides",
            "label": "slides-child"
        }))
        .await
        .unwrap();

    assert!(
        !result.success,
        "contract-gate must reject delivery when the workspace contract is not ready"
    );
    assert!(
        result.output.contains("workspace contract"),
        "failure must mention the workspace contract, got: {}",
        result.output
    );

    let tasks = supervisor.get_tasks_for_session("api:test-session");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].status, TaskStatus::Failed);
}

#[tokio::test]
async fn should_increment_depth_budget_per_level() {
    // Invariant #2: each level adds 1 to the child's `current`. We assert
    // it both via the pure increment path and via `child_tool()`.
    let dir = TempDir::new().unwrap();
    let memory = memory(&dir).await;

    let top = DepthBudget::top_level();
    assert_eq!(top.current, 0);

    let child_budget = top.increment().unwrap();
    assert_eq!(child_budget.current, 1);
    assert_eq!(child_budget.max, MAX_DEPTH);

    let grandchild_budget = child_budget.increment().unwrap();
    assert_eq!(grandchild_budget.current, 2);

    let refused = grandchild_budget
        .increment()
        .expect_err("past MAX_DEPTH must reject");
    match refused {
        HarnessError::DelegateDepthExceeded {
            depth,
            limit,
            message,
        } => {
            assert_eq!(depth, MAX_DEPTH);
            assert_eq!(limit, MAX_DEPTH);
            assert!(message.contains(&format!("depth {MAX_DEPTH}")));
        }
        other => panic!("expected DelegateDepthExceeded, got {other:?}"),
    }

    // Same propagation via the owning-tool helper: a top-level parent
    // produces a child tool at depth 1, which in turn hands out a depth-2
    // child, and the depth-2 child refuses to hand out another.
    let parent = DelegateTool::new(llm("ignored"), memory.clone(), PathBuf::from(dir.path()));
    let child = parent.child_tool().unwrap();
    assert_eq!(child.depth_budget(), child_budget);
    let grandchild = child.child_tool().unwrap();
    assert_eq!(grandchild.depth_budget(), grandchild_budget);
    match grandchild.child_tool() {
        Ok(_) => panic!("grandchild must refuse a fourth level"),
        Err(HarnessError::DelegateDepthExceeded { depth, .. }) => {
            assert_eq!(depth, MAX_DEPTH);
        }
        Err(other) => panic!("expected DelegateDepthExceeded, got {other:?}"),
    }
}

#[test]
fn should_serde_round_trip_depth_budget() {
    // Invariant #7: DepthBudget must serde-round-trip with stable fields.
    let budget = DepthBudget::at_level(1);
    let json = serde_json::to_string(&budget).unwrap();
    let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(raw["current"], 1);
    assert_eq!(raw["max"], MAX_DEPTH);
    let parsed: DepthBudget = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, budget);
}

#[test]
fn should_publish_delegated_deny_group_name_on_policy() {
    // Sanity check — the policy factory and the exported constant stay in
    // sync. ToolPolicy remains a plain struct; any future refactor would
    // want to see this test fail loudly.
    let policy: ToolPolicy = build_delegated_child_policy(Vec::new());
    assert_eq!(policy.deny, vec![DELEGATED_DENY_GROUP.to_string()]);
    assert!(policy.allow.is_empty());
}

#[tokio::test]
async fn should_route_delegation_event_through_tool_context_sink() {
    // M8.1 migration smoke test — DelegateTool is now a context-aware tool.
    // A tool instance constructed *without* `.with_harness_event_sink(...)`
    // must still emit its delegation event to the sink path carried by the
    // `ToolContext` when dispatched through `execute_with_context`. This
    // proves the tool actually reads from the typed context rather than
    // relying on its own builder-only wiring.
    use octos_agent::progress::SilentReporter;
    use octos_agent::tools::ToolContext;
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let memory = memory(&dir).await;
    let sink_path = dir.path().join("delegation-events.ndjson");

    // DepthBudget already exhausted so execute_with_context emits the
    // DepthExceeded event and returns. No child is spawned, which keeps the
    // test hermetic (no real LLM interaction required).
    let tool = DelegateTool::new(llm("unused"), memory, PathBuf::from(dir.path()))
        .with_depth_budget(DepthBudget::at_level(MAX_DEPTH));

    let ctx = ToolContext {
        tool_id: "m8.1-smoke".to_string(),
        reporter: Arc::new(SilentReporter),
        harness_event_sink: Some(sink_path.to_string_lossy().to_string()),
        attachment_paths: Vec::new(),
        audio_attachment_paths: Vec::new(),
        file_attachment_paths: Vec::new(),
        ..ToolContext::zero()
    };

    let result = tool
        .execute_with_context(&ctx, &serde_json::json!({"task": "ignored"}))
        .await;
    assert!(result.is_err(), "depth-exceeded must fail synchronously");

    let raw = std::fs::read_to_string(&sink_path)
        .expect("DelegateTool must write the depth-exceeded event to the context sink path");
    let entry: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
    assert_eq!(entry["kind"], "delegation");
    assert_eq!(entry["outcome"], "depth_exceeded");
}
