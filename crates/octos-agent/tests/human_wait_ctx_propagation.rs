//! UPCR-2026-023 live-soak BUG 1 — a human-blocking tool dispatched through the
//! agent batch executor must still see the per-turn `USER_QUESTION_CTX` /
//! `TOOL_APPROVAL_CTX` requester scoped around `process_message`.
//!
//! Regression being pinned: the batch executor runs every tool call through
//! `spawn_tool_task`, which `tokio::spawn`s the tool future onto a fresh task.
//! tokio task-locals are NOT inherited across `tokio::spawn`, and the spawned
//! task only re-scoped `TOOL_CTX` — never `USER_QUESTION_CTX` or
//! `TOOL_APPROVAL_CTX`. So a tool that reads either requester via `try_with`
//! found NONE inside the spawned task and degraded (e.g. `ask_user_question`
//! emitted its "no synchronous host response channel" text fallback even
//! though the serve turn handler had installed a `SessionUserQuestionRequester`).
//!
//! These tests drive a REAL `Agent` loop with a scripted `MockLlm`. The turn's
//! `process_message` future is wrapped in `USER_QUESTION_CTX.scope(...)` /
//! `TOOL_APPROVAL_CTX.scope(...)` exactly as the serve turn handler wraps it
//! (`ui_protocol.rs` run_standalone_turn). A probe tool reads the task-local at
//! execution time and records whether the requester was visible.
//!
//! RED before the fix: the probe records `requester_seen=false` (task-local
//! lost across the spawn) and `ask_user_question` returns its fallback.
//! GREEN after the fix: `spawn_tool_task` captures the requesters before the
//! spawn and re-scopes them around the tool execution, so the probe sees the
//! requester and `ask_user_question` blocks + returns the real answer.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use octos_agent::tools::{TOOL_APPROVAL_CTX, USER_QUESTION_CTX};
use octos_agent::{
    Agent, AgentConfig, AskUserQuestionTool, ConcurrencyClass, Tool, ToolApprovalDecision,
    ToolApprovalRequest, ToolApprovalRequester, ToolRegistry, ToolResult, UserQuestionOutcome,
    UserQuestionRequest, UserQuestionRequester,
};
use octos_core::ui_protocol::UserQuestionAnswer;
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

/// Probe tool: at execution time, records whether `USER_QUESTION_CTX` is
/// scoped. Stays `ConcurrencyClass::Safe` so a batch of only this tool runs
/// through the PARALLEL dispatch path (the live-soak shape).
struct UserQuestionProbeTool {
    seen: Arc<AtomicBool>,
    class: ConcurrencyClass,
}

#[async_trait]
impl Tool for UserQuestionProbeTool {
    fn name(&self) -> &str {
        "uq_probe"
    }
    fn description(&self) -> &str {
        "test-only: records whether USER_QUESTION_CTX is in scope at execute time"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn concurrency_class(&self) -> ConcurrencyClass {
        self.class
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        let seen = USER_QUESTION_CTX.try_with(|_| ()).is_ok();
        self.seen.store(seen, Ordering::SeqCst);
        Ok(ToolResult {
            output: format!("requester_seen={seen}"),
            success: true,
            ..Default::default()
        })
    }
}

/// Probe tool for the approval bridge: records whether `TOOL_APPROVAL_CTX` is
/// scoped at execute time. `Exclusive` so it forces the SERIAL dispatch path.
struct ApprovalProbeTool {
    seen: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for ApprovalProbeTool {
    fn name(&self) -> &str {
        "approval_probe"
    }
    fn description(&self) -> &str {
        "test-only: records whether TOOL_APPROVAL_CTX is in scope at execute time"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        let seen = TOOL_APPROVAL_CTX.try_with(|_| ()).is_ok();
        self.seen.store(seen, Ordering::SeqCst);
        Ok(ToolResult {
            output: format!("approval_seen={seen}"),
            success: true,
            ..Default::default()
        })
    }
}

/// Records that it was asked and replays a canned answer — the test stand-in
/// for the serve `SessionUserQuestionRequester`.
struct RecordingQuestionRequester {
    asked: Arc<AtomicBool>,
}

#[async_trait]
impl UserQuestionRequester for RecordingQuestionRequester {
    async fn request_user_question(&self, _request: UserQuestionRequest) -> UserQuestionOutcome {
        self.asked.store(true, Ordering::SeqCst);
        UserQuestionOutcome::Answered(vec![UserQuestionAnswer {
            selected_labels: vec!["axum".into()],
            free_text: None,
        }])
    }
}

struct AlwaysApprove;

#[async_trait]
impl ToolApprovalRequester for AlwaysApprove {
    async fn request_approval(&self, _request: ToolApprovalRequest) -> ToolApprovalDecision {
        ToolApprovalDecision::Approve
    }
}

struct MockLlm {
    responses: Mutex<Vec<ChatResponse>>,
}

impl MockLlm {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            eyre::bail!("MockLlm: no more scripted responses");
        }
        Ok(responses.remove(0))
    }
    fn context_window(&self) -> u32 {
        128_000
    }
    fn model_id(&self) -> &str {
        "mock-upcr-023-ctx"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn tool_use(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls: calls,
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 50,
            output_tokens: 5,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn end(text: &str) -> ChatResponse {
    ChatResponse {
        content: Some(text.into()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 5,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn tc(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        metadata: None,
    }
}

async fn agent_with(dir: &TempDir, tools: ToolRegistry, responses: Vec<ChatResponse>) -> Agent {
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlm::new(responses));
    Agent::new(AgentId::new("upcr-023-ctx"), llm, tools, memory).with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_batch_propagates_user_question_ctx_to_spawned_tool() {
    // PARALLEL path (Safe-only batch). The probe must see USER_QUESTION_CTX
    // scoped around process_message even though it runs inside spawn_tool_task's
    // tokio::spawn.
    let dir = TempDir::new().unwrap();
    let seen = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools.register(UserQuestionProbeTool {
        seen: seen.clone(),
        class: ConcurrencyClass::Safe,
    });

    let agent = agent_with(
        &dir,
        tools,
        vec![
            tool_use(vec![tc("probe", "uq_probe", serde_json::json!({}))]),
            end("done"),
        ],
    )
    .await;

    let requester: Arc<dyn UserQuestionRequester> = Arc::new(RecordingQuestionRequester {
        asked: Arc::new(AtomicBool::new(false)),
    });
    let resp = USER_QUESTION_CTX
        .scope(requester, agent.process_message("probe", &[], vec![]))
        .await
        .expect("agent loop must succeed");

    assert_eq!(resp.content, "done");
    assert!(
        seen.load(Ordering::SeqCst),
        "USER_QUESTION_CTX must be visible inside the spawned (parallel) tool task"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serial_batch_propagates_approval_ctx_to_spawned_tool() {
    // SERIAL path (Exclusive tool). The approval probe must see
    // TOOL_APPROVAL_CTX scoped around process_message even though it runs
    // inside spawn_tool_task's tokio::spawn.
    let dir = TempDir::new().unwrap();
    let seen = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools.register(ApprovalProbeTool { seen: seen.clone() });

    let agent = agent_with(
        &dir,
        tools,
        vec![
            tool_use(vec![tc("probe", "approval_probe", serde_json::json!({}))]),
            end("done"),
        ],
    )
    .await;

    let requester: Arc<dyn ToolApprovalRequester> = Arc::new(AlwaysApprove);
    let resp = TOOL_APPROVAL_CTX
        .scope(requester, agent.process_message("probe", &[], vec![]))
        .await
        .expect("agent loop must succeed");

    assert_eq!(resp.content, "done");
    assert!(
        seen.load(Ordering::SeqCst),
        "TOOL_APPROVAL_CTX must be visible inside the spawned (serial) tool task"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_user_question_blocks_through_batch_when_ctx_scoped() {
    // End-to-end: the REAL ask_user_question tool, dispatched through the agent
    // batch, must reach the scoped requester (block + return the answer) rather
    // than degrade to its "no synchronous host response channel" fallback.
    let dir = TempDir::new().unwrap();
    let asked = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools.register(AskUserQuestionTool::new());

    let agent = agent_with(
        &dir,
        tools,
        vec![
            tool_use(vec![tc(
                "auq",
                "ask_user_question",
                serde_json::json!({
                    "questions": [{
                        "header": "Framework",
                        "question": "Which web framework should I scaffold?",
                        "options": [
                            { "label": "axum", "description": "tower-based async" },
                            { "label": "actix", "description": "actor-based" }
                        ],
                        "multi_select": false
                    }]
                }),
            )]),
            end("done"),
        ],
    )
    .await;

    let requester: Arc<dyn UserQuestionRequester> = Arc::new(RecordingQuestionRequester {
        asked: asked.clone(),
    });
    let resp = USER_QUESTION_CTX
        .scope(requester, agent.process_message("ask", &[], vec![]))
        .await
        .expect("agent loop must succeed");

    assert_eq!(resp.content, "done");
    assert!(
        asked.load(Ordering::SeqCst),
        "the scoped requester must be asked (tool must NOT degrade to the no-channel fallback)"
    );
    let auq = resp
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("auq"))
        .expect("ask_user_question tool result recorded");
    assert!(
        auq.content.contains("answered") && auq.content.contains("axum"),
        "ask_user_question must record the REAL answer; got: {}",
        auq.content
    );
    assert!(
        !auq.content.contains("no synchronous host response channel"),
        "ask_user_question must NOT degrade to the unsupported fallback when a requester is scoped; got: {}",
        auq.content
    );
}
