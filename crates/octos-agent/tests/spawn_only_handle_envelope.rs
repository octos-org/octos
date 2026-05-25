//! M10 Phase 4 — agent context isolation.
//!
//! When a `spawn_only` tool is auto-backgrounded, the synthesized Tool
//! message returned to the LLM must be the JSON `task_handle` envelope, not
//! the full tool output. This pins the contract.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use octos_agent::tools::spawn::{BackgroundResultPayload, BackgroundResultSender};
use octos_agent::{Agent, AgentConfig, ReadTaskOutputTool, Tool, ToolRegistry, ToolResult};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

struct ScriptedLlm {
    responses: Mutex<Vec<ChatResponse>>,
    calls: Mutex<Vec<(Vec<Message>, ChatConfig)>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(Vec<Message>, ChatConfig)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.calls
            .lock()
            .unwrap()
            .push((messages.to_vec(), config.clone()));
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

struct FileOutputTool {
    name: &'static str,
    invocations: Arc<AtomicU32>,
    output_path: PathBuf,
}

#[async_trait]
impl Tool for FileOutputTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "spawn_only probe that writes a large text artifact"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        if let Some(parent) = self.output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &self.output_path,
            format!(
                "# Summary Probe\n\nALPHA_SENTINEL {}\n",
                "body ".repeat(1_200)
            ),
        )?;
        Ok(ToolResult {
            output: String::new(),
            success: true,
            files_to_send: vec![self.output_path.clone()],
            ..Default::default()
        })
    }
}

struct SendFileOkTool;

#[async_trait]
impl Tool for SendFileOkTool {
    fn name(&self) -> &str {
        "send_file"
    }

    fn description(&self) -> &str {
        "test send_file stub"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        Ok(ToolResult {
            output: "sent".to_string(),
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

    // Phase 4 gating: the spawn_only intercept emits the new task_handle
    // envelope only when `read_task_output` is registered (so legacy
    // chat/swarm registries that lack the reader keep their old free-text
    // message). Wire the reader so this test exercises the new path.
    let supervisor = tools.supervisor();
    let workspace = memory_dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    tools.register(ReadTaskOutputTool::new(
        supervisor,
        "test-session",
        None,
        workspace,
    ));

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

#[tokio::test]
async fn spawn_only_auto_summarize_appends_followup_for_large_file_output() {
    let memory_dir = TempDir::new().unwrap();
    let workspace = memory_dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let output_path = workspace.join("reports/summary_probe.md");

    let invocations = Arc::new(AtomicU32::new(0));
    let probe = FileOutputTool {
        name: "summary_probe",
        invocations: invocations.clone(),
        output_path: output_path.clone(),
    };

    let captured: Arc<Mutex<Vec<BackgroundResultPayload>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_sender = captured.clone();
    let sender: BackgroundResultSender = Arc::new(move |payload| {
        let captured = captured_for_sender.clone();
        async move {
            captured.lock().unwrap().push(payload);
            true
        }
        .boxed()
    });

    let mut tools = ToolRegistry::new();
    tools.set_workspace_root(workspace.clone());
    tools.register(probe);
    tools.register(SendFileOkTool);
    tools.mark_spawn_only("summary_probe", None);
    tools.mark_spawn_only_auto_summarize("summary_probe");
    tools.set_background_result_sender(sender);

    let supervisor = tools.supervisor();
    tools.register(ReadTaskOutputTool::new(
        supervisor,
        "test-session",
        None,
        workspace.clone(),
    ));

    let memory = Arc::new(
        EpisodeStore::open(memory_dir.path().join(".octos"))
            .await
            .unwrap(),
    );

    let scripted = Arc::new(ScriptedLlm::new(vec![
        tool_use(vec![tc("call-summary-1", "summary_probe")]),
        end_turn("Alpha summary remembers the generated report."),
    ]));
    let llm: Arc<dyn LlmProvider> = scripted.clone();

    let agent =
        Agent::new(AgentId::new("auto-summary"), llm, tools, memory).with_config(AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        });

    let response = agent
        .process_message("kick summary probe", &[], vec![])
        .await
        .expect("agent loop must not error");
    assert!(
        response.messages.iter().any(|m| {
            matches!(m.role, octos_core::MessageRole::Tool)
                && m.tool_call_id
                    .as_deref()
                    .is_some_and(|id| id.contains("call-summary-1"))
        }),
        "foreground turn should return the spawn_only handle"
    );

    let mut summary_payload = None;
    for _ in 0..100 {
        summary_payload = captured.lock().unwrap().iter().find_map(|payload| {
            payload
                .content
                .starts_with("`summary_probe` summary:")
                .then(|| payload.clone())
        });
        if summary_payload.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let summary_payload = summary_payload.unwrap_or_else(|| {
        panic!(
            "expected auto-summary background payload, got: {:#?}",
            captured.lock().unwrap()
        )
    });

    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert!(
        summary_payload
            .content
            .contains("Alpha summary remembers the generated report."),
        "got: {}",
        summary_payload.content
    );
    assert!(
        summary_payload
            .content
            .contains("Full output at reports/summary_probe.md; call `read_file` to inspect."),
        "got: {}",
        summary_payload.content
    );
    assert!(summary_payload.media.is_empty());
    assert!(summary_payload.envelope_media.is_empty());

    let calls = scripted.calls();
    assert!(
        calls.iter().any(|(messages, config)| {
            config.max_tokens == Some(300)
                && config.temperature == Some(0.0)
                && messages
                    .iter()
                    .any(|m| m.content.contains("ALPHA_SENTINEL"))
        }),
        "summary LLM call must be bounded and include file content; calls: {calls:#?}"
    );
}

// Codex P2 (round 1) regression guard: when `read_task_output` is NOT
// registered, the spawn_only intercept must fall back to the legacy
// free-text message instead of advertising a tool the LLM cannot call.
#[tokio::test]
async fn spawn_only_intercept_falls_back_to_legacy_text_without_reader() {
    let memory_dir = TempDir::new().unwrap();

    let invocations = Arc::new(AtomicU32::new(0));
    let probe = HugeOutputTool {
        name: "deep_research_probe_legacy",
        invocations: invocations.clone(),
        payload: "X".repeat(10_000),
    };

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    tools.mark_spawn_only("deep_research_probe_legacy", None);
    // Deliberately do NOT register `read_task_output` here — this
    // mirrors the chat / swarm registries.

    let memory = Arc::new(
        EpisodeStore::open(memory_dir.path().join(".octos"))
            .await
            .unwrap(),
    );

    let llm: Arc<dyn LlmProvider> = Arc::new(ScriptedLlm::new(vec![
        tool_use(vec![tc("call-legacy-1", "deep_research_probe_legacy")]),
        end_turn("done"),
    ]));

    let agent =
        Agent::new(AgentId::new("legacy-fallback"), llm, tools, memory).with_config(AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        });

    let response = agent
        .process_message("kick legacy", &[], vec![])
        .await
        .expect("agent loop must not error");

    let tool_msg = response
        .messages
        .iter()
        .find(|m| {
            matches!(m.role, octos_core::MessageRole::Tool)
                && m.tool_call_id
                    .as_deref()
                    .is_some_and(|id| id.contains("call-legacy-1"))
        })
        .expect("expected a Tool message for the spawn_only call");

    // Legacy free-text message still ends with "Output directory: …".
    // It must NOT be a JSON envelope advertising read_task_output —
    // that would mislead the LLM into calling a tool that isn't there.
    assert!(
        !tool_msg.content.contains("read_task_output"),
        "without read_task_output registered, the envelope must not advertise it; \
         got: {}",
        tool_msg.content
    );
    assert!(
        tool_msg.content.contains("Output directory:"),
        "expected legacy free-text fallback; got: {}",
        tool_msg.content
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
}
