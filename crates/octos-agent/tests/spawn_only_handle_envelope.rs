//! M10 Phase 4 — agent context isolation.
//!
//! When a `spawn_only` tool is auto-backgrounded, the synthesized Tool
//! message returned to the LLM must be the JSON `task_handle` envelope, not
//! the full tool output. This pins the contract.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, Tool, ToolRegistry, ToolResult};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

struct ScriptedLlm {
    responses: std::sync::Mutex<Vec<ChatResponse>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
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
        "handle-envelope-test"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Counting probe — the body produces a deliberately huge "result" string.
/// Pre-Phase 4 this large body would land in the LLM's tool-result message,
/// re-polluting context. Post-Phase 4 the LLM only sees the small handle
/// envelope.
struct HugeOutputTool {
    name: &'static str,
    invocations: Arc<AtomicU32>,
    payload: String,
}

#[async_trait]
impl Tool for HugeOutputTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "spawn_only probe with large output"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            output: self.payload.clone(),
            success: true,
            ..Default::default()
        })
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
async fn spawn_only_intercept_returns_task_handle_envelope_not_full_output() {
    let memory_dir = TempDir::new().unwrap();

    // Tool produces a 50KB output to mirror a real deep_search report.
    let big_payload = "X".repeat(50_000);
    let invocations = Arc::new(AtomicU32::new(0));
    let probe = HugeOutputTool {
        name: "deep_research_probe",
        invocations: invocations.clone(),
        payload: big_payload.clone(),
    };

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    tools.mark_spawn_only("deep_research_probe", None);

    let memory = Arc::new(
        EpisodeStore::open(memory_dir.path().join(".octos"))
            .await
            .unwrap(),
    );

    let llm: Arc<dyn LlmProvider> = Arc::new(ScriptedLlm::new(vec![
        tool_use(vec![tc("call-handle-1", "deep_research_probe")]),
        // The agent's spawn_only intercept ends the foreground turn at the
        // first hit; this second response is here only as a safety net.
        end_turn("done"),
    ]));

    let agent =
        Agent::new(AgentId::new("handle-envelope"), llm, tools, memory).with_config(AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        });

    let response = agent
        .process_message("kick deep_research", &[], vec![])
        .await
        .expect("agent loop must not error");

    // Find the tool message returned for our spawn_only call.
    let tool_msg = response
        .messages
        .iter()
        .find(|m| {
            matches!(m.role, octos_core::MessageRole::Tool)
                && m.tool_call_id
                    .as_deref()
                    .is_some_and(|id| id.contains("call-handle-1"))
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a Tool message for the spawn_only call; messages: {:#?}",
                response.messages
            )
        });

    // 1. The full payload must NOT be inlined into the LLM's tool result.
    assert!(
        !tool_msg.content.contains(&big_payload),
        "spawn_only Tool message must not inline the full tool output"
    );

    // 2. The Tool message body is < 1KB (acceptance criterion).
    assert!(
        tool_msg.content.len() < 1024,
        "spawn_only Tool message must stay under 1KB; got {} bytes",
        tool_msg.content.len()
    );

    // 3. The body parses as JSON with the documented `task_handle`
    //    envelope shape.
    let envelope: serde_json::Value = serde_json::from_str(&tool_msg.content)
        .expect("spawn_only Tool message must be a JSON object");
    assert_eq!(envelope["ok"], true);
    assert!(
        envelope["task_handle"].is_string(),
        "envelope must carry a task_handle string"
    );
    assert_eq!(envelope["read_with"], "read_task_output");
    assert!(envelope["expected_files"].is_array());
    assert!(envelope["summary"].is_string());

    // Settle any spurious background tasks before we tear down.
    tokio::time::sleep(Duration::from_millis(50)).await;
}
