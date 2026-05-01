//! Tests for the spawn_only ToolPolicy gate.
//!
//! Origin: PR #688 (run_pipeline → spawn_only) shipped two latent bypasses:
//!
//! - **MEDIUM #3**: the spawn_only intercept site in
//!   `crates/octos-agent/src/agent/execution.rs` runs BEFORE the registry's
//!   provider-policy check. A denied stale tool call was silently
//!   `tokio::spawn`ed; the deny only surfaced async, inside the background
//!   task. The foreground turn observed a fake "started successfully" and
//!   the LLM had no signal to stop retrying.
//!
//! - **MEDIUM #4**: the gateway / session ActorFactory path registers
//!   `run_pipeline` AFTER the base `tool_policy` was applied, so a
//!   `tool_policy.deny: ["run_pipeline"]` configured globally was ignored
//!   on gateway-spawned actors. (The CLI `chat.rs` path does not have this
//!   bug because it applies policy AFTER mark_spawn_only.)
//!
//! These tests pin Option A: a synchronous policy check at the spawn_only
//! intercept (covers MEDIUM #3) and the re-application of `tool_policy`
//! after `mark_spawn_only` registration (covered separately in
//! `octos-cli`'s session_actor tests; here we cover the registry-level
//! contract that makes that re-application meaningful).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::tools::ToolPolicy;
use octos_agent::{Agent, AgentConfig, Tool, ToolRegistry, ToolResult};
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
        "policy-test"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Tool that records every invocation. Used as a probe to assert the
/// spawn_only intercept did NOT actually run the tool when policy denies.
struct CountingTool {
    name: &'static str,
    invocations: Arc<AtomicU32>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "spawn_only probe"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
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

// =========================================================================
// MEDIUM #3: spawn_only intercept must enforce provider policy.
//
// Setup:
//   - Register a tool "blocked_bg".
//   - Mark it spawn_only.
//   - Set a provider_policy that denies it.
//   - Script the LLM to call it once.
//
// Expected:
//   - The agent's foreground turn returns a synthetic Tool message tagged
//     "[POLICY DENIED]" — visible to the LLM in the same turn.
//   - The tool's `execute` body is NEVER invoked. Without the fix the tool
//     runs in `tokio::spawn` and only fails async after the foreground
//     turn already accepted the spawn.
// =========================================================================

#[tokio::test]
async fn policy_denies_run_pipeline_via_spawn_only_path() {
    let memory_dir = TempDir::new().unwrap();

    let invocations = Arc::new(AtomicU32::new(0));
    let probe = CountingTool {
        name: "blocked_bg",
        invocations: invocations.clone(),
    };

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    tools.mark_spawn_only("blocked_bg", None);

    // Deny the spawn_only tool via provider policy.
    let policy = ToolPolicy {
        deny: vec!["blocked_bg".to_string()],
        ..Default::default()
    };
    tools.set_provider_policy(policy);

    let memory = Arc::new(
        EpisodeStore::open(memory_dir.path().join(".octos"))
            .await
            .unwrap(),
    );

    let llm: Arc<dyn LlmProvider> = Arc::new(ScriptedLlm::new(vec![
        tool_use(vec![tc("call-1", "blocked_bg")]),
        end_turn("done"),
    ]));

    let agent = Agent::new(AgentId::new("policy-spawn-only"), llm, tools, memory).with_config(
        AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        },
    );

    let response = agent
        .process_message("kick blocked spawn_only", &[], vec![])
        .await
        .expect("agent loop should not error on policy deny");

    // The foreground turn must surface the deny synchronously: there is a
    // Tool message in the turn's outbound messages tagged with the
    // [POLICY DENIED] marker the intercept produces.
    let denied = response.messages.iter().any(|m| {
        matches!(m.role, octos_core::MessageRole::Tool)
            && m.content.contains("[POLICY DENIED]")
            && m.content.contains("blocked_bg")
    });
    assert!(
        denied,
        "expected a synchronous [POLICY DENIED] Tool message; got: {:#?}",
        response.messages
    );

    // Settle any spurious background tasks (there should be none).
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The tool body must NEVER have been invoked. Without the policy gate
    // at the intercept site this assertion fails because the spawn_only
    // branch fires `tokio::spawn` before checking the policy.
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "spawn_only tool body must not run when provider policy denies it"
    );
}

// =========================================================================
// Companion: confirm the registry's apply_policy correctly removes a
// spawn_only-marked tool when invoked AFTER mark_spawn_only.
//
// This is the registry-level contract that the session_actor.rs
// re-application (PR #688 follow-up MEDIUM #4 fix) depends on:
//   `apply_policy` must drop a denied tool even if it was already marked
//   spawn_only — otherwise the re-application after run_pipeline
//   registration would be a no-op.
// =========================================================================

#[test]
fn apply_policy_after_mark_spawn_only_removes_denied_tool() {
    let probe = CountingTool {
        name: "lateral_pipeline",
        invocations: Arc::new(AtomicU32::new(0)),
    };

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    tools.mark_spawn_only("lateral_pipeline", None);

    // Sanity: tool is registered before policy.
    assert!(tools.get("lateral_pipeline").is_some());

    // Mirror the session_actor.rs re-application: apply a deny-list
    // policy AFTER the spawn_only-marked tool was registered.
    let policy = ToolPolicy {
        deny: vec!["lateral_pipeline".to_string()],
        ..Default::default()
    };
    tools.apply_policy(&policy);

    // The tool must be evicted by apply_policy regardless of its
    // spawn_only marker. Without this, MEDIUM #4's re-application after
    // ActorFactory registration would not remove `run_pipeline`.
    assert!(
        tools.get("lateral_pipeline").is_none(),
        "apply_policy must drop a denied tool even when spawn_only"
    );

    // Codex review (PR #688 follow-up): `apply_policy` must ALSO clean
    // up the parallel `spawn_only` marker. Otherwise a stale call to
    // the now-evicted tool would still trip the spawn_only intercept in
    // `execution.rs`, fall through to the background `tokio::spawn`,
    // and only fail async — exactly the "fake started" pattern the
    // policy gate is meant to prevent.
    assert!(
        !tools.is_spawn_only("lateral_pipeline"),
        "apply_policy must also drop the spawn_only marker for evicted tools"
    );
}

// =========================================================================
// MEDIUM #4 + codex follow-up: stale spawn_only call after `apply_policy`.
//
// Setup:
//   - Register a tool, mark it spawn_only.
//   - `apply_policy` denies it (this is the gateway/session_actor path
//     where `run_pipeline` was registered first then policy applied).
//   - Script the LLM to call the now-removed tool (a stale call).
//
// Expected:
//   - The spawn_only marker has been cleaned up (no fake "background
//     started" message).
//   - The tool body is never invoked.
//   - The agent loop returns an "unknown tool" error message rather
//     than spawning an async task that fails later.
// =========================================================================

#[tokio::test]
async fn apply_policy_then_stale_call_fails_synchronously_not_async() {
    let memory_dir = TempDir::new().unwrap();

    let invocations = Arc::new(AtomicU32::new(0));
    let probe = CountingTool {
        name: "stale_pipeline",
        invocations: invocations.clone(),
    };

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    tools.mark_spawn_only("stale_pipeline", None);

    // Mirror the session_actor.rs MEDIUM #4 fix: `apply_policy` runs
    // AFTER the spawn_only-marked tool was registered. The fix in
    // `retain` must clean up both the tool AND the spawn_only marker.
    let policy = ToolPolicy {
        deny: vec!["stale_pipeline".to_string()],
        ..Default::default()
    };
    tools.apply_policy(&policy);

    let memory = Arc::new(
        EpisodeStore::open(memory_dir.path().join(".octos"))
            .await
            .unwrap(),
    );

    let llm: Arc<dyn LlmProvider> = Arc::new(ScriptedLlm::new(vec![
        tool_use(vec![tc("call-stale", "stale_pipeline")]),
        end_turn("done"),
    ]));

    let agent =
        Agent::new(AgentId::new("policy-stale"), llm, tools, memory).with_config(AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        });

    let response = agent
        .process_message("call stale tool", &[], vec![])
        .await
        .expect("agent loop must not error");

    // Settle any spurious background tasks (there should be none).
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The tool body must NEVER have been invoked. Without the `retain`
    // fix the stale spawn_only marker survives, the intercept fires,
    // and the tool runs in `tokio::spawn` (then fails async).
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "stale spawn_only call must not invoke the (denied) tool body"
    );

    // Codex review (round 2): also verify the LLM saw a real synchronous
    // failure for the call (unknown-tool / error), NOT a fake
    // "Pipeline started in background" success message. The exact
    // wording of the spawn_only success message is "Pipeline started in
    // background" — its presence here would mean the intercept fooled
    // the foreground turn.
    // The agent loop normalises tool_call_ids ("call-stale" gets a
    // "call_" prefix); match by tool_call_id suffix to stay robust.
    let tool_msg = response
        .messages
        .iter()
        .find(|m| {
            matches!(m.role, octos_core::MessageRole::Tool)
                && m.tool_call_id
                    .as_deref()
                    .is_some_and(|id| id.contains("call-stale"))
        })
        .unwrap_or_else(|| {
            panic!(
                "a Tool message for call-stale must exist; got messages: {:#?}",
                response.messages
            )
        });
    assert!(
        !tool_msg.content.contains("started in background"),
        "stale call must not produce a fake 'started in background' \
         message; got: {}",
        tool_msg.content
    );
    // The reply must look like a synchronous failure (unknown tool,
    // policy denial, or registry error). We accept any of these
    // markers so the assertion is robust to minor rewording.
    let looks_like_error = tool_msg.content.contains("unknown tool")
        || tool_msg.content.contains("[POLICY DENIED]")
        || tool_msg.content.contains("error")
        || tool_msg.content.contains("denied");
    assert!(
        looks_like_error,
        "stale spawn_only call must surface a synchronous error to the \
         LLM; got: {}",
        tool_msg.content
    );
}
