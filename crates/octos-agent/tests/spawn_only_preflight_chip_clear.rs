//! Regression: spawn_only early-return failure paths must clear the
//! activity chip.
//!
//! Origin: reproduced live on the mini5 soak. When a `spawn_only` tool call
//! fails at the *synchronous* interception stage (provider-policy deny,
//! pre-flight validation failure, or a before-tool hook deny) the foreground
//! turn early-returns the synthetic failure `Message` to the LLM — but the
//! matching `ProgressEvent::ToolCompleted` is never emitted. The chip the
//! `ToolStarted` event lit (`Orchestrating… (1 active) · Using <tool> <id>`)
//! never clears, so the TUI shows a phantom active tool forever even though
//! the turn is `✓ Done` and zero background tasks are running.
//!
//! The interception closure in `crates/octos-agent/src/agent/execution.rs`
//! emits `ToolStarted` once, then for the normal completion paths emits a
//! matching `ToolCompleted`. The early-return failure branches between the
//! `ToolStarted` emit and the background-spawn point skipped that emit.
//!
//! Contract pinned here: **every `ToolStarted` must have a matching
//! `ToolCompleted` carrying the same `tool_id`** — for the
//! pre-flight-validation-failure path and the provider-policy-deny path.
//! Both fail synchronously, so the chip must clear in the same turn.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::tools::ToolPolicy;
use octos_agent::{
    Agent, AgentConfig, ProgressEvent, ProgressReporter, Tool, ToolRegistry, ToolResult,
};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

// =========================================================================
// Test infra
// =========================================================================

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
        "chip-clear-test"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Records the events the chip-rendering layer consumes. We capture both
/// `ToolStarted` (lights the chip) and `ToolCompleted` (clears it) keyed by
/// `tool_id`, plus the `ToolCompleted.success` flag.
#[derive(Default)]
struct ChipReporter {
    started: std::sync::Mutex<Vec<String>>,           // tool_id
    completed: std::sync::Mutex<Vec<(String, bool)>>, // (tool_id, success)
}

impl ProgressReporter for ChipReporter {
    fn report(&self, event: ProgressEvent) {
        match event {
            ProgressEvent::ToolStarted { tool_id, .. } => {
                self.started.lock().unwrap().push(tool_id);
            }
            ProgressEvent::ToolCompleted {
                tool_id, success, ..
            } => {
                self.completed.lock().unwrap().push((tool_id, success));
            }
            _ => {}
        }
    }
}

impl ChipReporter {
    /// Returns every `tool_id` that lit the chip (`ToolStarted`) but never
    /// cleared it (no matching `ToolCompleted`). A non-empty result is a chip
    /// leak.
    fn leaked_chips(&self) -> Vec<String> {
        let started = self.started.lock().unwrap();
        let completed = self.completed.lock().unwrap();
        started
            .iter()
            .filter(|id| !completed.iter().any(|(cid, _)| cid == *id))
            .cloned()
            .collect()
    }
}

/// Tool whose `execute` records invocations and whose `pre_flight_validate`
/// can be configured to fail.
struct ConfigurableTool {
    name: &'static str,
    invocations: Arc<AtomicU32>,
    preflight_fail: bool,
}

#[async_trait]
impl Tool for ConfigurableTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "spawn_only chip-clear probe"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn pre_flight_validate(&self, _args: &serde_json::Value) -> Result<(), String> {
        if self.preflight_fail {
            Err("unknown pipeline 'bogus'".to_string())
        } else {
            Ok(())
        }
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            output: format!("{} ran\n", self.name),
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

async fn open_memory(dir: &TempDir) -> Arc<EpisodeStore> {
    Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap())
}

// =========================================================================
// Pre-flight-validation-failure path must clear the chip.
//
// A spawn_only tool whose `pre_flight_validate` returns `Err` early-returns
// a `[VALIDATION FAILED]` Tool message. The fix emits a matching
// `ToolCompleted{ success: false }` first so the chip clears.
// =========================================================================

#[tokio::test]
async fn preflight_failure_emits_tool_completed_to_clear_chip() {
    let memory_dir = TempDir::new().unwrap();

    let invocations = Arc::new(AtomicU32::new(0));
    let probe = ConfigurableTool {
        name: "preflight_bg",
        invocations: invocations.clone(),
        preflight_fail: true,
    };

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    tools.mark_spawn_only("preflight_bg", None);

    let memory = open_memory(&memory_dir).await;
    let llm: Arc<dyn LlmProvider> = Arc::new(ScriptedLlm::new(vec![
        tool_use(vec![tc("call-pf", "preflight_bg")]),
        end_turn("handled the validation error inline"),
    ]));

    let reporter = Arc::new(ChipReporter::default());
    let agent = Agent::new(AgentId::new("preflight-chip"), llm, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        })
        .with_reporter(reporter.clone());

    let response = agent
        .process_message("kick preflight-failing spawn_only", &[], vec![])
        .await
        .expect("agent loop should not error on pre-flight failure");

    // Sanity: the LLM saw a synchronous [VALIDATION FAILED] result.
    assert!(
        response.messages.iter().any(|m| {
            matches!(m.role, octos_core::MessageRole::Tool)
                && m.content.contains("[VALIDATION FAILED]")
        }),
        "expected a synchronous [VALIDATION FAILED] Tool message; got: {:#?}",
        response.messages
    );

    // The tool body must never have run (pre-flight rejected it).
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "pre-flight failure must not invoke the tool body"
    );

    // THE CHIP CONTRACT: every ToolStarted must have a matching
    // ToolCompleted. On unfixed HEAD the pre-flight early-return skips the
    // ToolCompleted emit, so the chip leaks.
    let leaked = reporter.leaked_chips();
    assert!(
        leaked.is_empty(),
        "pre-flight failure leaked an activity chip (ToolStarted with no \
         matching ToolCompleted) for tool_id(s): {:?}",
        leaked
    );

    // And the clearing event must mark the call as failed (success:false),
    // not a phantom success.
    let completed = reporter.completed.lock().unwrap().clone();
    assert!(
        completed.iter().any(|(_, success)| !success),
        "pre-flight failure must emit ToolCompleted{{ success: false }}; \
         saw: {:?}",
        completed
    );
}

// =========================================================================
// Provider-policy-deny path must clear the chip.
//
// A spawn_only tool denied by provider policy early-returns a
// `[POLICY DENIED]` Tool message. The fix emits a matching
// `ToolCompleted{ success: false }` first so the chip clears.
// =========================================================================

#[tokio::test]
async fn policy_deny_emits_tool_completed_to_clear_chip() {
    let memory_dir = TempDir::new().unwrap();

    let invocations = Arc::new(AtomicU32::new(0));
    let probe = ConfigurableTool {
        name: "denied_bg",
        invocations: invocations.clone(),
        preflight_fail: false,
    };

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    tools.mark_spawn_only("denied_bg", None);

    // Deny the spawn_only tool via provider policy (this fires at the
    // intercept before the background spawn).
    let policy = ToolPolicy {
        deny: vec!["denied_bg".to_string()],
        ..Default::default()
    };
    tools.set_provider_policy(policy);

    let memory = open_memory(&memory_dir).await;
    let llm: Arc<dyn LlmProvider> = Arc::new(ScriptedLlm::new(vec![
        tool_use(vec![tc("call-deny", "denied_bg")]),
        end_turn("handled the denial inline"),
    ]));

    let reporter = Arc::new(ChipReporter::default());
    let agent = Agent::new(AgentId::new("policy-chip"), llm, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        })
        .with_reporter(reporter.clone());

    let response = agent
        .process_message("kick denied spawn_only", &[], vec![])
        .await
        .expect("agent loop should not error on policy deny");

    // Sanity: the LLM saw a synchronous [POLICY DENIED] result.
    assert!(
        response.messages.iter().any(|m| {
            matches!(m.role, octos_core::MessageRole::Tool) && m.content.contains("[POLICY DENIED]")
        }),
        "expected a synchronous [POLICY DENIED] Tool message; got: {:#?}",
        response.messages
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "policy-denied spawn_only tool body must not run"
    );

    // THE CHIP CONTRACT: every ToolStarted has a matching ToolCompleted.
    let leaked = reporter.leaked_chips();
    assert!(
        leaked.is_empty(),
        "policy deny leaked an activity chip (ToolStarted with no matching \
         ToolCompleted) for tool_id(s): {:?}",
        leaked
    );

    let completed = reporter.completed.lock().unwrap().clone();
    assert!(
        completed.iter().any(|(_, success)| !success),
        "policy deny must emit ToolCompleted{{ success: false }}; saw: {:?}",
        completed
    );
}
