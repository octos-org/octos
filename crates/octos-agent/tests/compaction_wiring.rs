//! Integration tests for Review A F-002: compaction runner wiring.
//!
//! These tests cover the seam between `Agent::with_compaction_runner` and
//! `CompactionRunner::with_provider` so that:
//!   - an agent constructed with a compaction policy actually runs compaction
//!     (no silent no-op), and
//!   - when the policy declares `CompactionSummarizerKind::LlmIterative`,
//!     the LLM-iterative summarizer is selected (not the extractive fallback).
//!
//! Run with `cargo test -p octos-agent --test compaction_wiring`.
//!
//! `classify_report` is the canonical entry point into the harness error
//! layer. For these tests we deliberately keep the setup minimal: an
//! `ExtractiveSummarizer` (deterministic, no LLM) to prove the
//! plumbing works, and a scripted LLM to prove the `LlmIterative`
//! path selects an LLM-iterative summarizer.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::compaction::{CompactionPhase, CompactionRunner};
use octos_agent::workspace_policy::{
    CompactionPolicy, CompactionSummarizerKind, WorkspaceArtifactsPolicy, WorkspacePolicy,
    WorkspacePolicyKind, WorkspacePolicyWorkspace, WorkspaceSnapshotTrigger,
    WorkspaceTrackingPolicy, WorkspaceVersionControlPolicy, WorkspaceVersionControlProvider,
};
use octos_agent::{Agent, COMPACTION_POLICY_SCHEMA_VERSION, WORKSPACE_POLICY_SCHEMA_VERSION};
use octos_core::{AgentId, Message, MessageRole};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;

fn system_msg(content: &str) -> Message {
    Message {
        role: MessageRole::System,
        content: content.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        timestamp: chrono::Utc::now(),
    }
}

fn user_msg(content: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: content.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        timestamp: chrono::Utc::now(),
    }
}

fn assistant_msg(content: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: content.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        timestamp: chrono::Utc::now(),
    }
}

/// Scripted LLM provider that records how many times `chat()` was called.
///
/// Used to prove the `LlmIterative` summarizer path actually reaches the LLM
/// rather than silently falling back to the extractive summarizer.
struct SpyLlm {
    calls: Mutex<u32>,
}

impl SpyLlm {
    fn new() -> Self {
        Self {
            calls: Mutex::new(0),
        }
    }

    fn call_count(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl LlmProvider for SpyLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        // Return a minimally valid SessionSummary JSON so the iterative
        // summarizer's parser succeeds — it doesn't matter what the content
        // is; we only need to prove the LLM call happens.
        let payload = serde_json::json!({
            "schema_version": 1,
            "goal": "wire compaction",
            "constraints": [],
            "progress_done": [],
            "progress_in_progress": [],
            "decisions": [],
            "files": [],
            "next_steps": []
        })
        .to_string();
        Ok(ChatResponse {
            content: Some(payload),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn model_id(&self) -> &str {
        "spy-llm"
    }

    fn provider_name(&self) -> &str {
        "spy"
    }
}

fn workspace_with_compaction(policy: CompactionPolicy) -> WorkspacePolicy {
    WorkspacePolicy {
        schema_version: WORKSPACE_POLICY_SCHEMA_VERSION,
        workspace: WorkspacePolicyWorkspace {
            kind: WorkspacePolicyKind::Sites,
        },
        version_control: WorkspaceVersionControlPolicy {
            provider: WorkspaceVersionControlProvider::Git,
            auto_init: false,
            trigger: WorkspaceSnapshotTrigger::TurnEnd,
            fail_on_error: false,
        },
        tracking: WorkspaceTrackingPolicy { ignore: vec![] },
        validation: Default::default(),
        artifacts: WorkspaceArtifactsPolicy {
            entries: BTreeMap::from([("primary".into(), "output/deck.pptx".into())]),
        },
        spawn_tasks: BTreeMap::new(),
        compaction: Some(policy),
    }
}

async fn build_agent() -> Agent {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let memory = Arc::new(
        EpisodeStore::open(tmp.path())
            .await
            .expect("open episode store"),
    );
    let llm: Arc<dyn LlmProvider> = Arc::new(SpyLlm::new());
    let tools = octos_agent::ToolRegistry::new();
    Agent::new(AgentId::new("test-compaction-wiring"), llm, tools, memory)
}

#[tokio::test]
async fn should_wire_compaction_runner_when_policy_declares_preflight() {
    // F-002 check #1: Agent::with_compaction_runner must be callable and the
    // resulting runner exposed via compaction_runner() so the loop's
    // maybe_run_preflight_compaction path is no longer a no-op.
    let compaction_policy = CompactionPolicy {
        schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
        token_budget: 1_000,
        preflight_threshold: Some(200),
        prune_tool_results_after_turns: None,
        preserved_artifacts: vec!["primary".into()],
        preserved_invariants: vec![],
        summarizer: CompactionSummarizerKind::Extractive,
    };
    let workspace = workspace_with_compaction(compaction_policy.clone());
    let runner = CompactionRunner::new(compaction_policy).with_workspace_policy(&workspace);

    let agent = build_agent()
        .await
        .with_compaction_runner(Arc::new(runner))
        .with_compaction_workspace(workspace);

    let wired = agent
        .compaction_runner()
        .expect("compaction runner must be wired after with_compaction_runner");
    assert_eq!(
        wired.summarizer_kind(),
        "extractive",
        "extractive policy must select the extractive summarizer"
    );
    assert!(
        agent.compaction_workspace().is_some(),
        "workspace policy must round-trip to the agent",
    );

    // Drive the wired runner through a preflight pass on a history that
    // exceeds the threshold to prove compaction actually fires — i.e. the
    // loop's maybe_run_preflight_compaction would no longer early-return.
    let big = "x".repeat(3_000);
    let mut messages = vec![
        system_msg("system prompt"),
        user_msg(&format!("long user message {big}")),
        assistant_msg(&format!("long assistant reply {big}")),
        user_msg("trigger"),
    ];
    assert!(
        wired.needs_preflight(&messages).is_some(),
        "with_compaction_runner must preserve needs_preflight signalling"
    );
    let outcome = wired.run(&mut messages, CompactionPhase::Preflight);
    assert!(
        outcome.performed,
        "preflight should execute compaction when the runner is wired"
    );
}

#[tokio::test]
async fn should_use_llm_summarizer_when_configured() {
    // F-002 check #2: CompactionRunner::with_provider MUST select the
    // LLM-iterative summarizer when the policy declares it — before the fix,
    // default_summarizer_for(LlmIterative) silently returned the extractive
    // fallback even when a provider was wired.
    let compaction_policy = CompactionPolicy {
        schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
        // Budget large enough that system+recent fits but old-message summary
        // still has to run. Preflight threshold is well below tokens_before so
        // the runner enters the summary branch that actually invokes the
        // wired summarizer.
        token_budget: 8_000,
        preflight_threshold: Some(1_000),
        prune_tool_results_after_turns: None,
        preserved_artifacts: vec![],
        preserved_invariants: vec![],
        summarizer: CompactionSummarizerKind::LlmIterative,
    };
    let workspace = workspace_with_compaction(compaction_policy.clone());

    let spy = Arc::new(SpyLlm::new());
    let provider: Arc<dyn LlmProvider> = spy.clone();
    let runner = CompactionRunner::with_provider(compaction_policy, provider)
        .with_workspace_policy(&workspace);

    assert_eq!(
        runner.summarizer_kind(),
        "llm_iterative",
        "with_provider + LlmIterative must select the llm_iterative summarizer",
    );

    // Build a conversation long enough for the recent-boundary heuristic to
    // leave old messages on the compactable side (MIN_RECENT_MESSAGES = 6),
    // so the summarizer path actually runs rather than falling into the
    // oldest-first trim branch.
    let filler = "word ".repeat(400);
    let mut messages = vec![system_msg("system prompt")];
    for i in 0..14 {
        messages.push(user_msg(&format!("turn {i} user question {filler}")));
        messages.push(assistant_msg(&format!("turn {i} assistant reply {filler}")));
    }
    messages.push(user_msg("trigger preflight"));

    assert!(
        runner.needs_preflight(&messages).is_some(),
        "LLM-iterative policy with preflight_threshold must still gate compaction on overflow"
    );

    let outcome = runner.run(&mut messages, CompactionPhase::Preflight);
    assert!(
        outcome.performed,
        "preflight should compact something when the conversation overflows the budget"
    );

    assert!(
        spy.call_count() >= 1,
        "LlmIterative summarizer must have called the wired provider at least once; got {}",
        spy.call_count()
    );
}

#[tokio::test]
async fn should_keep_legacy_no_op_when_no_runner_wired() {
    // Guard: without a compaction runner, the agent must still expose None
    // so maybe_run_preflight_compaction stays a no-op for every caller that
    // hasn't opted into M6.3.
    let agent = build_agent().await;
    assert!(
        agent.compaction_runner().is_none(),
        "fresh agent must not have a compaction runner wired"
    );
    assert!(
        agent.compaction_workspace().is_none(),
        "fresh agent must not have a compaction workspace wired"
    );
}
