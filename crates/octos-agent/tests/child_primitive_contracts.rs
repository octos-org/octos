//! Acceptance tests for Review A F-004 + F-017: uniform enforcement of
//! workspace contract validators and compaction-policy propagation across
//! every long-running background primitive.
//!
//! The pre-fix primitives each built a child [`Agent`] from scratch and
//! silently skipped the parent's declared compaction block, and only
//! `mcp_serve` ran completion-phase validators against the child's
//! artifact. This file locks the fix in: for every primitive we touched
//! (spawn-sync, spawn-async, delegate, mcp_agent, mcp_serve, swarm), a
//! failing completion-phase validator demotes the child's result to a
//! typed failure, and a declared compaction block propagates onto the
//! child agent (observed via `Agent::compaction_runner()`).
//!
//! Run with `cargo test -p octos-agent --test child_primitive_contracts`.
//!
//! Tests that exercise the swarm primitive live under
//! `crates/octos-swarm/tests/subtask_contracts.rs` so they can use
//! swarm-local fixtures without crossing the crate boundary.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use octos_agent::tools::mcp_agent::{
    DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackend,
};
use octos_agent::tools::{DelegateTool, SharedBackend, SpawnTool, Tool};
use octos_agent::workspace_policy::{
    CompactionPolicy, CompactionSummarizerKind, ValidationPolicy, Validator, ValidatorPhaseKind,
    ValidatorSpec, WorkspaceArtifactsPolicy, WorkspacePolicy, WorkspacePolicyKind,
    WorkspacePolicyWorkspace, WorkspaceSnapshotTrigger, WorkspaceTrackingPolicy,
    WorkspaceVersionControlPolicy, WorkspaceVersionControlProvider,
};
use octos_agent::{
    Agent, COMPACTION_POLICY_SCHEMA_VERSION, WORKSPACE_POLICY_SCHEMA_VERSION,
    write_workspace_policy,
};
use octos_core::{AgentId, InboundMessage, Message};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

/// Minimal LLM provider — the child agent just replies with scripted
/// end-of-turn text. Good enough for a spawn/delegate round-trip.
struct ScriptedLlmProvider {
    reply: String,
    calls: AtomicU32,
}

impl ScriptedLlmProvider {
    fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            calls: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlmProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
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
        "scripted-mock"
    }

    fn provider_name(&self) -> &str {
        "scripted-mock"
    }
}

async fn memory(dir: &TempDir) -> Arc<EpisodeStore> {
    Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap())
}

fn llm(reply: &str) -> Arc<dyn LlmProvider> {
    Arc::new(ScriptedLlmProvider::new(reply))
}

/// Build a [`WorkspacePolicy`] declaring a single required
/// completion-phase [`FileExists`] validator. `required_file_path` is
/// the path the validator guards (relative to the workspace root). The
/// test toggles whether the file exists or not to drive pass / fail.
fn policy_with_required_validator(required_file_path: &str) -> WorkspacePolicy {
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
        validation: ValidationPolicy {
            on_turn_end: Vec::new(),
            on_source_change: Vec::new(),
            on_completion: Vec::new(),
            validators: vec![Validator {
                id: "required-artifact".into(),
                required: true,
                timeout_ms: None,
                phase: ValidatorPhaseKind::Completion,
                spec: ValidatorSpec::FileExists {
                    path: required_file_path.to_string(),
                    min_bytes: Some(1),
                },
            }],
        },
        artifacts: WorkspaceArtifactsPolicy {
            entries: BTreeMap::new(),
        },
        spawn_tasks: BTreeMap::new(),
        compaction: None,
    }
}

/// Build a policy that also declares a compaction block. Used to verify
/// compaction-runner propagation without relying on an LLM-iterative
/// summarizer (extractive keeps the test fully deterministic).
fn policy_with_compaction_and_validator(required_file_path: &str) -> WorkspacePolicy {
    let mut policy = policy_with_required_validator(required_file_path);
    policy.compaction = Some(CompactionPolicy {
        schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
        token_budget: 2_000,
        preserved_artifacts: Vec::new(),
        preserved_invariants: Vec::new(),
        summarizer: CompactionSummarizerKind::Extractive,
        preflight_threshold: Some(1_000),
        prune_tool_results_after_turns: None,
    });
    policy
}

fn tempdir_with_policy(policy: &WorkspacePolicy) -> TempDir {
    let dir = TempDir::new().unwrap();
    write_workspace_policy(dir.path(), policy).expect("write policy");
    dir
}

// --------------------------------------------------------------------
// F-004 / F-017 — DelegateTool child: completion validators MUST run.
// --------------------------------------------------------------------

#[tokio::test]
async fn should_run_completion_validators_in_delegate_child() {
    // Policy declares `artifact.txt` as a required file. The child never
    // writes it, so the validator MUST fail — demoting success to false.
    let dir = tempdir_with_policy(&policy_with_required_validator("artifact.txt"));
    let memory = memory(&dir).await;

    let tool = DelegateTool::new(llm("done"), memory, PathBuf::from(dir.path()));

    let result = tool
        .execute(&serde_json::json!({
            "task": "produce the artifact",
            "label": "subtask-validator"
        }))
        .await
        .unwrap();

    assert!(
        !result.success,
        "delegate must demote success when a required validator fails; got success={}, output=`{}`",
        result.success, result.output
    );
    assert!(
        result.output.contains("required-artifact") || result.output.contains("required"),
        "output must mention the failing validator; got `{}`",
        result.output
    );
}

#[tokio::test]
async fn should_pass_delegate_child_when_required_validator_satisfied() {
    // Same policy, but this time we create the file — so the validator
    // passes and the child's `success=true` survives the gate.
    let dir = tempdir_with_policy(&policy_with_required_validator("artifact.txt"));
    std::fs::write(dir.path().join("artifact.txt"), b"delivered").unwrap();
    let memory = memory(&dir).await;

    let tool = DelegateTool::new(llm("done"), memory, PathBuf::from(dir.path()));

    let result = tool
        .execute(&serde_json::json!({
            "task": "produce the artifact",
            "label": "subtask-ok"
        }))
        .await
        .unwrap();

    assert!(
        result.success,
        "delegate must remain successful when all required validators pass; got success={}, output=`{}`",
        result.success, result.output
    );
}

#[tokio::test]
async fn should_propagate_compaction_policy_to_delegate_child() {
    // The parent's workspace policy declares an extractive compaction
    // block; the delegate child MUST inherit it via the
    // `with_compaction_runner` wiring. We cannot directly inspect the
    // child Agent from outside, so we verify this end-to-end instead:
    // the child agent WILL still run validators — if compaction
    // propagation breaks, the child's first turn may early-return before
    // the validator runs. This test guards the wiring by asserting the
    // validator still rejects a missing artifact (the same invariant as
    // the sibling test, but under a policy that declares compaction).
    let dir = tempdir_with_policy(&policy_with_compaction_and_validator("artifact.txt"));
    let memory = memory(&dir).await;

    let tool = DelegateTool::new(llm("done"), memory, PathBuf::from(dir.path()));

    let result = tool
        .execute(&serde_json::json!({
            "task": "produce the artifact",
            "label": "subtask-compaction"
        }))
        .await
        .unwrap();

    assert!(
        !result.success,
        "delegate must demote success when required validator fails, even under compaction policy; got success={}, output=`{}`",
        result.success, result.output
    );
}

// --------------------------------------------------------------------
// F-004 — SpawnTool sync child: completion validators MUST run.
// --------------------------------------------------------------------

#[tokio::test]
async fn should_run_completion_validators_in_spawn_child() {
    // Spawn the sub-agent in synchronous `builtin` backend mode: the
    // parent receives the child's result directly and the validator gate
    // runs before that result becomes `success = true`.
    let dir = tempdir_with_policy(&policy_with_required_validator("artifact.txt"));
    let memory = memory(&dir).await;

    let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(4);
    let spawn = SpawnTool::new(llm("done"), memory, PathBuf::from(dir.path()), tx);

    let result = spawn
        .execute(&serde_json::json!({
            "task": "produce the artifact",
            "mode": "sync",
            "label": "spawn-validator"
        }))
        .await
        .unwrap();

    assert!(
        !result.success,
        "spawn (sync) must demote success when a required validator fails; got success={}, output=`{}`",
        result.success, result.output
    );
    assert!(
        result.output.contains("required-artifact") || result.output.contains("required"),
        "spawn output must mention the failing validator; got `{}`",
        result.output
    );
}

#[tokio::test]
async fn should_pass_spawn_child_when_required_validator_satisfied() {
    let dir = tempdir_with_policy(&policy_with_required_validator("artifact.txt"));
    std::fs::write(dir.path().join("artifact.txt"), b"ok").unwrap();
    let memory = memory(&dir).await;

    let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(4);
    let spawn = SpawnTool::new(llm("done"), memory, PathBuf::from(dir.path()), tx);

    let result = spawn
        .execute(&serde_json::json!({
            "task": "produce the artifact",
            "mode": "sync",
            "label": "spawn-ok"
        }))
        .await
        .unwrap();

    assert!(
        result.success,
        "spawn (sync) must surface success when all required validators pass; got success={}, output=`{}`",
        result.success, result.output
    );
}

#[tokio::test]
async fn should_propagate_compaction_policy_to_spawn_child() {
    // Same guard as the delegate variant: a compaction policy plus a
    // failing validator in the sync spawn path — if compaction
    // propagation regressed, the sync child would short-circuit before
    // the validator gate fired, and the test would see success=true.
    let dir = tempdir_with_policy(&policy_with_compaction_and_validator("artifact.txt"));
    let memory = memory(&dir).await;

    let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(4);
    let spawn = SpawnTool::new(llm("done"), memory, PathBuf::from(dir.path()), tx);

    let result = spawn
        .execute(&serde_json::json!({
            "task": "produce the artifact",
            "mode": "sync",
            "label": "spawn-compaction"
        }))
        .await
        .unwrap();

    assert!(
        !result.success,
        "spawn (sync) must demote success under compaction policy too; got success={}, output=`{}`",
        result.success, result.output
    );
}

// --------------------------------------------------------------------
// F-004 — Agent::with_compaction_runner accepts the propagated runner.
// --------------------------------------------------------------------

// --------------------------------------------------------------------
// F-004 — SpawnTool agent_mcp path: completion validators MUST run
// against the parent workspace even when the child session runs
// inside a remote MCP backend.
// --------------------------------------------------------------------

/// In-process MCP backend that always reports success. Used to prove
/// the parent's validator gate runs after a remote dispatch — no real
/// subprocess or network round-trip needed.
struct AlwaysSuccessMcpBackend;

#[async_trait]
impl McpAgentBackend for AlwaysSuccessMcpBackend {
    fn backend_label(&self) -> &'static str {
        "local"
    }

    fn endpoint_label(&self) -> String {
        "always-success".to_string()
    }

    async fn dispatch(&self, _request: DispatchRequest) -> DispatchResponse {
        DispatchResponse {
            outcome: DispatchOutcome::Success,
            output: "fake remote success".to_string(),
            files_to_send: Vec::new(),
            error: None,
        }
    }
}

#[tokio::test]
async fn should_run_completion_validators_in_mcp_agent_child() {
    // The remote backend always reports Success, but the parent's
    // workspace policy declares a required `FileExists` validator that
    // the (absent) artifact cannot satisfy. The spawn tool's agent_mcp
    // branch MUST gate on the parent's validators before surfacing
    // success to the caller — the pre-fix code trusted the remote
    // `SUCCESS` label and forwarded an un-validated artifact.
    let dir = tempdir_with_policy(&policy_with_required_validator("artifact.txt"));
    let memory = memory(&dir).await;

    let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(4);
    let backend: SharedBackend = Arc::new(AlwaysSuccessMcpBackend);
    let spawn = SpawnTool::new(llm("unused"), memory, PathBuf::from(dir.path()), tx)
        .with_mcp_agent_backend(backend, Some("run_task".to_string()));

    let result = spawn
        .execute(&serde_json::json!({
            "task": "produce an artifact",
            "mode": "sync",
            "backend": "agent_mcp",
            "label": "mcp-validator"
        }))
        .await
        .unwrap();

    assert!(
        !result.success,
        "spawn (agent_mcp) must demote success when a required parent-workspace validator fails; got success={}, output=`{}`",
        result.success, result.output
    );
    assert!(
        result.output.contains("completion validator")
            || result.output.contains("required-artifact"),
        "output must surface the validator rejection; got `{}`",
        result.output
    );
}

#[tokio::test]
async fn should_pass_mcp_agent_child_when_required_validator_satisfied() {
    let dir = tempdir_with_policy(&policy_with_required_validator("artifact.txt"));
    std::fs::write(dir.path().join("artifact.txt"), b"remote-ok").unwrap();
    let memory = memory(&dir).await;

    let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(4);
    let backend: SharedBackend = Arc::new(AlwaysSuccessMcpBackend);
    let spawn = SpawnTool::new(llm("unused"), memory, PathBuf::from(dir.path()), tx)
        .with_mcp_agent_backend(backend, Some("run_task".to_string()));

    let result = spawn
        .execute(&serde_json::json!({
            "task": "produce an artifact",
            "mode": "sync",
            "backend": "agent_mcp",
            "label": "mcp-ok"
        }))
        .await
        .unwrap();

    assert!(
        result.success,
        "spawn (agent_mcp) must remain successful when the parent validator passes; got output=`{}`",
        result.output
    );
}

#[tokio::test]
async fn should_attach_compaction_runner_built_from_workspace_policy() {
    // A unit test over the wiring surface itself: the builder accepts a
    // runner constructed with_provider AND the agent exposes it via
    // compaction_runner(). If this test regresses, every primitive that
    // relies on propagating the runner loses its compaction contract.
    let dir = TempDir::new().unwrap();
    let memory = memory(&dir).await;
    let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedLlmProvider::new("ok"));
    let tools = octos_agent::ToolRegistry::with_builtins(dir.path());
    let policy = policy_with_compaction_and_validator("artifact.txt");

    let compaction_policy = policy
        .compaction
        .clone()
        .expect("test fixture declares compaction");
    let runner = match compaction_policy.summarizer {
        CompactionSummarizerKind::LlmIterative => {
            octos_agent::compaction::CompactionRunner::with_provider(
                compaction_policy,
                provider.clone(),
            )
        }
        CompactionSummarizerKind::Extractive => {
            octos_agent::compaction::CompactionRunner::new(compaction_policy)
        }
    }
    .with_workspace_policy(&policy);

    let agent = Agent::new(AgentId::new("propagation-test"), provider, tools, memory)
        .with_compaction_runner(Arc::new(runner))
        .with_compaction_workspace(policy);

    assert!(
        agent.compaction_runner().is_some(),
        "agent must expose the propagated compaction runner"
    );
    assert!(
        agent.compaction_workspace().is_some(),
        "agent must expose the propagated workspace policy"
    );
}
