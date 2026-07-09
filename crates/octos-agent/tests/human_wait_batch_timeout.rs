//! UPCR-2026-023 — a tool batch containing a human-wait tool
//! (`Tool::blocks_on_human_input() == true`, e.g. `ask_user_question`) must
//! run with NO finite batch-level timeout.
//!
//! Regression being pinned: the agent batch dispatcher wraps the spawned tool
//! tasks in `tokio::time::timeout(compute_batch_timeout_secs(...), ...)`. Before
//! this fix `compute_batch_timeout_secs` returned a finite ceiling even when a
//! human-wait tool was present, so once that ceiling fired the dispatcher
//! returned synthetic `"… timed out after N seconds"` results AND dropped the
//! still-running `JoinHandle`s — detaching the human-wait task so its
//! `PendingQuestionWaiterGuard` never dropped (pending question leaked and was
//! later replayed as a stale prompt).
//!
//! These tests drive a REAL `Agent` loop with a scripted `MockLlm` and a
//! deliberately tiny `tool_timeout_secs` / `default_interactive_tool_timeout_secs`
//! (1s). A human-wait tool whose "human" answers AFTER that ceiling must still
//! return its real answer rather than a synthetic timeout — proving the batch
//! layer no longer bounds it. Both dispatch shapes are exercised:
//!
//! - parallel: a human-wait tool is `ConcurrencyClass::Safe`, so a batch of
//!   only-Safe tools dispatches through the parallel (`join_all`) path.
//! - serial: pairing the human-wait tool with an `Exclusive` peer forces the
//!   serial (per-call) dispatch path.
//!
//! RED before this fix (the batch ceiling is finite and fires after 1s, producing
//! `"timed out"`); GREEN after `compute_batch_timeout_secs` yields `None` for a
//! human-wait batch and both dispatch paths await the handles directly.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, ConcurrencyClass, Tool, ToolRegistry, ToolResult};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

/// A human-wait tool: blocks on a "human" (a `Notify` tripped after `delay`)
/// before returning its answer. Declares `blocks_on_human_input() == true`,
/// mirroring `ask_user_question`. The delay is comfortably longer than the
/// batch ceiling the test configures, so a surviving batch timeout would turn
/// the result into a synthetic `"timed out"` message.
struct HumanWaitTool {
    delay: Duration,
}

#[async_trait]
impl Tool for HumanWaitTool {
    fn name(&self) -> &str {
        "human_wait"
    }
    fn description(&self) -> &str {
        "test-only: blocks on a human (delayed Notify) before answering"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn blocks_on_human_input(&self) -> bool {
        true
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        // Emulate the human taking longer than any finite batch ceiling.
        tokio::time::sleep(self.delay).await;
        Ok(ToolResult {
            output: "human answered".into(),
            success: true,
            ..Default::default()
        })
    }
}

/// A fast, side-effect-free reader. Stays `Safe`, so a batch of
/// {human_wait, fast_reader} dispatches through the PARALLEL path.
struct FastSafeTool;

#[async_trait]
impl Tool for FastSafeTool {
    fn name(&self) -> &str {
        "fast_reader"
    }
    fn description(&self) -> &str {
        "test-only: fast safe reader"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        Ok(ToolResult {
            output: "read ok".into(),
            success: true,
            ..Default::default()
        })
    }
}

/// An `Exclusive` tool. Pairing it with the human-wait tool forces the SERIAL
/// dispatch path (any-Exclusive batch). It is fast and side-effect-free for
/// the test, but reports `Exclusive` so the executor serializes the batch.
struct ExclusiveFastTool;

#[async_trait]
impl Tool for ExclusiveFastTool {
    fn name(&self) -> &str {
        "exclusive_fast"
    }
    fn description(&self) -> &str {
        "test-only: exclusive but fast"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        Ok(ToolResult {
            output: "exclusive ok".into(),
            success: true,
            ..Default::default()
        })
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
        "mock-upcr-023"
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

/// Capture the tool-result messages the agent recorded so we can assert NONE
/// of them are the synthetic batch-timeout message.
fn assert_no_timeout_message(history: &[Message], answered_id: &str) {
    let timed_out: Vec<&Message> = history
        .iter()
        .filter(|m| m.content.contains("timed out after"))
        .collect();
    assert!(
        timed_out.is_empty(),
        "no tool result should be a synthetic batch-timeout; got: {:?}",
        timed_out.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    let answered = history.iter().any(|m| {
        m.tool_call_id.as_deref() == Some(answered_id) && m.content.contains("human answered")
    });
    assert!(
        answered,
        "human-wait tool must record its REAL answer (not detach/timeout) for \
         tool_call_id={answered_id}; history={:?}",
        history
            .iter()
            .map(|m| (m.tool_call_id.clone(), m.content.clone()))
            .collect::<Vec<_>>()
    );
}

/// Build an agent whose batch ceiling is a tiny 1s — short enough that a
/// surviving batch timeout would fire well before the human-wait tool's 2s
/// "human" answers.
async fn agent_with_tiny_batch_timeout(
    dir: &TempDir,
    tools: ToolRegistry,
    responses: Vec<ChatResponse>,
) -> Agent {
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlm::new(responses));
    Agent::new(AgentId::new("upcr-023"), llm, tools, memory).with_config(AgentConfig {
        save_episodes: false,
        // Tiny ceilings: a surviving batch timeout fires at 1s, before the
        // 2s human answer. The fix must make the human-wait batch unbounded.
        tool_timeout_secs: 1,
        default_interactive_tool_timeout_secs: 1,
        ..Default::default()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_batch_with_human_wait_is_not_timed_out_at_batch_layer() {
    // PARALLEL path: human_wait (Safe) alone → join_all dispatch. With a 1s
    // batch ceiling and a 2s "human", the pre-fix code returned a synthetic
    // "timed out" and detached the task. The fix awaits join_all directly.
    let dir = TempDir::new().unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(HumanWaitTool {
        delay: Duration::from_secs(2),
    });

    let agent = agent_with_tiny_batch_timeout(
        &dir,
        tools,
        vec![
            tool_use(vec![tc("hw_call", "human_wait", serde_json::json!({}))]),
            end("done"),
        ],
    )
    .await;

    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        agent.process_message("ask the human", &[], vec![]),
    )
    .await
    .expect("agent loop must not hang")
    .expect("agent loop must succeed");

    assert_eq!(resp.content, "done");
    assert_no_timeout_message(&resp.messages, "hw_call");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serial_batch_with_human_wait_is_not_timed_out_at_batch_layer() {
    // SERIAL path: human_wait + an Exclusive peer → any-Exclusive serial
    // dispatch. The human-wait call must still NOT be bounded by the batch
    // ceiling; it returns its real answer after 2s under a 1s ceiling.
    let dir = TempDir::new().unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(HumanWaitTool {
        delay: Duration::from_secs(2),
    });
    tools.register(ExclusiveFastTool);

    let agent = agent_with_tiny_batch_timeout(
        &dir,
        tools,
        vec![
            tool_use(vec![
                tc("hw_call", "human_wait", serde_json::json!({})),
                tc("ex_call", "exclusive_fast", serde_json::json!({})),
            ]),
            end("done"),
        ],
    )
    .await;

    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        agent.process_message("ask the human then mutate", &[], vec![]),
    )
    .await
    .expect("agent loop must not hang")
    .expect("agent loop must succeed");

    assert_eq!(resp.content, "done");
    assert_no_timeout_message(&resp.messages, "hw_call");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_parallel_batch_human_wait_unbounded_fast_peer_completes() {
    // MIXED PARALLEL batch: human_wait (Safe, 2s) + fast_reader (Safe). The
    // batch is unbounded at the batch layer (so the human-wait survives the 1s
    // ceiling), while the fast peer still completes normally — proving the
    // no-timeout decision does not break the rest of the batch.
    let dir = TempDir::new().unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(HumanWaitTool {
        delay: Duration::from_secs(2),
    });
    tools.register(FastSafeTool);

    let agent = agent_with_tiny_batch_timeout(
        &dir,
        tools,
        vec![
            tool_use(vec![
                tc("hw_call", "human_wait", serde_json::json!({})),
                tc("fast_call", "fast_reader", serde_json::json!({})),
            ]),
            end("done"),
        ],
    )
    .await;

    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        agent.process_message("ask + read", &[], vec![]),
    )
    .await
    .expect("agent loop must not hang")
    .expect("agent loop must succeed");

    assert_eq!(resp.content, "done");
    assert_no_timeout_message(&resp.messages, "hw_call");
    let fast_ok = resp
        .messages
        .iter()
        .any(|m| m.tool_call_id.as_deref() == Some("fast_call") && m.content.contains("read ok"));
    assert!(
        fast_ok,
        "the fast Safe peer must still complete normally in a mixed human-wait batch"
    );
}
