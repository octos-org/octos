//! Whole-chain regression: an ID-less provider (empty `tool_call_id`) must
//! still route a spawn_only background FAILURE back as a recovery signal.
//!
//! Production context (session slides-1780072199773-2htqt1, mini3,
//! 2026-05-29): kimi / MiniMax via wisemodel stream tool calls with no `id`.
//! Before the streaming-layer id synthesis in `agent/streaming.rs`, the
//! empty `tool_call_id` made `TaskSupervisor::notify_failure` skip the
//! `SpawnOnlyFailureSignal`, so a failed background skill (e.g. `mofa_slides`
//! parse-erroring on a TOML the model itself wrote) never routed a recovery
//! turn back to the LLM and the model could not self-correct.
//!
//! This drives the REAL agent loop end to end — streaming assembly
//! (id synthesis) -> spawn_only synth-ack -> background dispatch -> failure
//! -> `notify_failure` -> `SpawnOnlyFailureSignal`. The `ScriptedLlm`'s
//! `ChatResponse` carries an empty `tool_call_id`; the default `chat_stream`
//! re-emits it as a `ToolCallDelta` consumed by `consume_stream_inner`, which
//! is exactly where the synthesized `call_{index}` id is minted. The signal
//! is what the session actor / WS path turns into a recovery turn (covered
//! separately by `session_actor` tests for non-empty ids), so asserting the
//! signal fires closes the empty-id gap.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, ReadTaskOutputTool, Tool, ToolRegistry, ToolResult};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

struct ScriptedLlm {
    responses: Mutex<Vec<ChatResponse>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut r = self.responses.lock().unwrap();
        if r.is_empty() {
            eyre::bail!("ScriptedLlm: no more responses");
        }
        Ok(r.remove(0))
    }
    fn context_window(&self) -> u32 {
        128_000
    }
    fn model_id(&self) -> &str {
        "empty-id-recovery-test"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// A spawn_only probe whose background execution always fails — mirrors a
/// `mofa_slides` run that hits a TOML parse error after being backgrounded.
struct FailingSpawnOnlyTool;

#[async_trait]
impl Tool for FailingSpawnOnlyTool {
    fn name(&self) -> &str {
        "failing_probe"
    }
    fn description(&self) -> &str {
        "spawn_only probe that always fails in the background"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        eyre::bail!("simulated TOML parse failure at line 40")
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

fn end_turn(text: &str) -> ChatResponse {
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

fn tc(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: serde_json::json!({}),
        metadata: None,
    }
}

#[tokio::test]
async fn empty_id_spawn_only_failure_still_fires_recovery_signal() {
    let memory_dir = TempDir::new().unwrap();

    let mut tools = ToolRegistry::new();
    tools.register(FailingSpawnOnlyTool);
    tools.mark_spawn_only("failing_probe", None);

    // The new task_handle envelope path is gated on `read_task_output` being
    // registered (otherwise the legacy free-text ack is used). Wire it so the
    // test exercises the production-shaped path.
    let supervisor = tools.supervisor();
    let workspace = memory_dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    tools.register(ReadTaskOutputTool::new(
        supervisor.clone(),
        "test-session",
        None,
        workspace,
    ));

    // Capture (tool_name, error_message) for any SpawnOnlyFailureSignal the
    // supervisor emits. This is the signal the session actor / WS path turns
    // into a recovery turn.
    let captured: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    supervisor.set_on_failure_signal(move |signal| {
        sink.lock()
            .unwrap()
            .push((signal.tool_name.clone(), signal.error_message.clone()));
    });

    let memory = Arc::new(
        EpisodeStore::open(memory_dir.path().join(".octos"))
            .await
            .unwrap(),
    );

    // Turn 1: the model calls the spawn_only tool with an EMPTY tool_call_id
    // — the kimi / MiniMax via wisemodel regime. The default `chat_stream`
    // re-emits it as a `ToolCallDelta`, and `consume_stream_inner` mints the
    // synthesized `call_0` id. Turn 2 is a safety net (the foreground turn
    // ends at the spawn_only ack).
    let llm: Arc<dyn LlmProvider> = Arc::new(ScriptedLlm::new(vec![
        tool_use(vec![tc("", "failing_probe")]),
        end_turn("acknowledged the failure"),
    ]));

    let agent = Agent::new(AgentId::new("empty-id-recovery"), llm, tools, memory).with_config(
        AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        },
    );

    let _ = agent
        .process_message("kick the failing probe", &[], vec![])
        .await
        .expect("agent loop must not error");

    // Let the detached background task fail and the signal propagate.
    for _ in 0..100 {
        if !captured.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let signals = captured.lock().unwrap();
    assert!(
        !signals.is_empty(),
        "spawn_only background failure must fire a recovery signal even when the \
         provider emitted an EMPTY tool_call_id — the synthesized id must reach \
         the supervisor's synth-ack lookup. (Empty here means notify_failure \
         silently skipped and the model would never get a recovery turn.)"
    );
    assert_eq!(
        signals[0].0, "failing_probe",
        "signal must name the failed tool"
    );
    assert!(
        signals[0].1.contains("simulated TOML parse failure"),
        "signal must carry the background failure text; got: {}",
        signals[0].1
    );
}
