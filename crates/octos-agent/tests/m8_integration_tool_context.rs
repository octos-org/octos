//! M8.8 / M8.2 / M8.4 reconciliation tests (item 1 of fix-first checklist).
//!
//! These tests pin the regression that landed in feature/m8.8-concurrent-scheduler:
//! the rewritten `Agent::spawn_tool_task` (formerly `execute_tools`) rebuilds
//! `ToolContext` from `ToolContext::zero()` and silently drops the M8.2
//! `agent_definitions` registry and the M8.4 `file_state_cache` handle. The
//! integration branch must thread both fields into the foreground and
//! spawn-only `ToolContext` builders so:
//!
//! - `spawn` calls with `agent_definition_id` resolve against the live
//!   registry instead of seeing an empty zero-value default.
//! - `read_file` called twice through the agent path can return the
//!   `[FILE_UNCHANGED]` short-circuit (proof that the cache reached the
//!   tool).
//! - spawn-only background tools see a `ToolContext` with the same M8 fields
//!   populated as the foreground path (proof the M8.8 reorganisation did
//!   not silently zero them in the background branch).
//!
//! The tests use scripted `MockLlm` responses — they never call out to a
//! real provider — so they run in milliseconds on CI.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{
    Agent, AgentConfig, FileStateCache, Tool, ToolRegistry, ToolResult,
    agents::{AgentDefinition, AgentDefinitions},
    tools::ToolContext,
};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

/// Tool that captures the `ToolContext` it was invoked with so the test can
/// assert which M8 fields the executor actually populated.
struct CtxProbeTool {
    name: &'static str,
    captured: Arc<Mutex<Option<CapturedCtx>>>,
}

#[derive(Clone)]
struct CapturedCtx {
    agent_definition_ids: Vec<String>,
    file_state_cache_present: bool,
    /// Whether the captured `ToolContext.permissions` permits each
    /// of the named tools. Used by the gap-4b coverage to prove the
    /// profile-derived envelope reaches the call site.
    permissions_for: std::collections::HashMap<String, bool>,
}

impl CtxProbeTool {
    fn new(name: &'static str) -> (Self, Arc<Mutex<Option<CapturedCtx>>>) {
        let captured = Arc::new(Mutex::new(None));
        (
            Self {
                name,
                captured: captured.clone(),
            },
            captured,
        )
    }
}

#[async_trait]
impl Tool for CtxProbeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "ctx probe"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        // Legacy path — the agent will call `execute_with_context`. Empty
        // capture so the test sees None when the typed path is bypassed.
        Ok(ToolResult {
            output: "ok".into(),
            success: true,
            ..Default::default()
        })
    }
    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        _args: &serde_json::Value,
    ) -> eyre::Result<ToolResult> {
        let mut perms = std::collections::HashMap::new();
        for tool in ["read_file", "write_file", "shell", "edit_file"] {
            perms.insert(tool.to_string(), ctx.permissions.is_tool_allowed(tool));
        }
        let captured = CapturedCtx {
            agent_definition_ids: ctx.agent_definitions.ids().map(|s| s.to_string()).collect(),
            file_state_cache_present: ctx.file_state_cache.is_some(),
            permissions_for: perms,
        };
        *self.captured.lock().unwrap() = Some(captured);
        Ok(ToolResult {
            output: "ok".into(),
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
        "mock-m8-fix"
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

#[tokio::test]
async fn threads_agent_definitions_into_spawn_tool_after_m8_8_scheduler() {
    // Build an agent with a non-empty AgentDefinitions registry and a
    // ctx-probe tool. The probe captures the typed ToolContext it was
    // invoked with. Assert the probe sees the registry the agent was
    // configured with — proving the M8.8 executor rewrite did not zero it.
    let dir = TempDir::new().unwrap();
    let (probe, captured) = CtxProbeTool::new("probe_tool");

    let mut registry = AgentDefinitions::new();
    let manifest = AgentDefinition::from_json_str(
        r#"{
            "name": "test-worker",
            "version": 1,
            "tools": ["read_file", "grep"]
        }"#,
    )
    .expect("parse manifest");
    registry.insert("test-worker", manifest);

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlm::new(vec![
        tool_use(vec![tc("call_1", "probe_tool", serde_json::json!({}))]),
        end("done"),
    ]));

    let agent = Agent::new(AgentId::new("m8-fix-test"), llm, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_agent_definitions(Arc::new(registry));

    let resp = agent
        .process_message("invoke probe", &[], vec![])
        .await
        .expect("agent loop must succeed");
    assert_eq!(resp.content, "done");

    let captured = captured
        .lock()
        .unwrap()
        .clone()
        .expect("probe must have observed a ToolContext");
    assert!(
        captured
            .agent_definition_ids
            .contains(&"test-worker".to_string()),
        "ToolContext.agent_definitions did not see the registry the agent was \
         configured with — M8.8 reconciliation regressed: ids={:?}",
        captured.agent_definition_ids
    );
}

#[tokio::test]
async fn threads_file_state_cache_into_read_file_after_m8_8_scheduler() {
    // Read the same file twice through a real Agent loop; the second read
    // must hit the [FILE_UNCHANGED] short-circuit. That requires the
    // foreground ToolContext built by the M8.8 executor to carry the cache
    // forward from `Agent::file_state_cache`.
    let workspace = TempDir::new().unwrap();
    let memory_dir = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();

    let cache = Arc::new(FileStateCache::new());
    let mut tools = ToolRegistry::new();
    tools.register(octos_agent::ReadFileTool::new(workspace.path()));
    let memory = Arc::new(
        EpisodeStore::open(memory_dir.path().join(".octos"))
            .await
            .unwrap(),
    );
    let read_call_1 = tc(
        "call_1",
        "read_file",
        serde_json::json!({"path": "notes.txt"}),
    );
    let read_call_2 = tc(
        "call_2",
        "read_file",
        serde_json::json!({"path": "notes.txt"}),
    );
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlm::new(vec![
        tool_use(vec![read_call_1]),
        tool_use(vec![read_call_2]),
        end("done"),
    ]));

    let agent = Agent::new(AgentId::new("m8-fix-cache"), llm, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_file_state_cache(cache.clone());

    let resp = agent
        .process_message("read twice", &[], vec![])
        .await
        .expect("agent loop must succeed");
    assert_eq!(resp.content, "done");

    // Walk the recorded messages: there must be exactly two `Tool` results
    // for read_file, the first containing the full file body, the second
    // containing the FILE_UNCHANGED stub. If the M8.8 reconciliation
    // regressed, both reads would return the body and the cache would be
    // empty.
    let tool_outputs: Vec<&str> = resp
        .messages
        .iter()
        .filter(|m| matches!(m.role, octos_core::MessageRole::Tool))
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        tool_outputs.len(),
        2,
        "expected two tool results, got {}: {:?}",
        tool_outputs.len(),
        tool_outputs
    );
    assert!(
        tool_outputs[0].contains("alpha"),
        "first read should return file body, got: {}",
        tool_outputs[0]
    );
    assert!(
        tool_outputs[1].contains("[FILE_UNCHANGED]"),
        "second read must short-circuit via the file_state_cache after \
         M8.8 reconciliation, got: {}",
        tool_outputs[1]
    );
    assert_eq!(
        cache.len(),
        1,
        "cache must contain the read entry to prove threading worked"
    );
}

#[tokio::test]
async fn spawn_only_background_path_receives_full_tool_context_after_m8_8_scheduler() {
    // The M8.8 rewrite split the spawn-only background branch into a fresh
    // `ToolContext` builder that started from `ToolContext::zero()`. The
    // reconciliation must thread both `agent_definitions` and
    // `file_state_cache` into that builder. We exercise the path by
    // marking the probe tool as spawn_only and asserting the background
    // branch's ctx carries the M8 fields.
    let dir = TempDir::new().unwrap();
    let (probe, captured) = CtxProbeTool::new("bg_probe");

    let mut registry = AgentDefinitions::new();
    registry.insert(
        "bg-worker",
        AgentDefinition::from_json_str(
            r#"{
                "name": "bg-worker",
                "version": 1,
                "tools": ["read_file"]
            }"#,
        )
        .expect("parse manifest"),
    );
    let cache = Arc::new(FileStateCache::new());

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    tools.mark_spawn_only("bg_probe", None);
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlm::new(vec![
        tool_use(vec![tc("call_1", "bg_probe", serde_json::json!({}))]),
        end("done"),
    ]));

    let agent = Agent::new(AgentId::new("m8-fix-spawn-only"), llm, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            // Suppress auto-send to keep the test cheap; the spawn-only
            // ctx assertion is the load-bearing check.
            suppress_auto_send_files: true,
            ..Default::default()
        })
        .with_agent_definitions(Arc::new(registry))
        .with_file_state_cache(cache.clone());

    // Agent loop runs the spawn_only branch synchronously up to the
    // `tokio::spawn` for the background body; the body itself runs after
    // the loop returns. Give the background task a moment to invoke the
    // probe and stash its ctx capture.
    let _ = agent
        .process_message("kick spawn-only probe", &[], vec![])
        .await
        .expect("agent loop must succeed");

    // Wait for the background tokio task to invoke the probe. Probe is
    // synchronous and writes the capture before its async body returns,
    // but the `tokio::spawn` itself is detached.
    for _ in 0..50 {
        if captured.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let captured = captured
        .lock()
        .unwrap()
        .clone()
        .expect("spawn-only background probe must have observed a ToolContext");
    assert!(
        captured
            .agent_definition_ids
            .contains(&"bg-worker".to_string()),
        "spawn-only background ToolContext.agent_definitions was empty — \
         M8.8 reconciliation must populate it: ids={:?}",
        captured.agent_definition_ids
    );
    assert!(
        captured.file_state_cache_present,
        "spawn-only background ToolContext.file_state_cache was None — \
         M8.8 reconciliation must populate it"
    );
}

#[tokio::test]
async fn threads_profile_permissions_into_tool_context_after_m8_fix_8() {
    // M8 fix-first item 8 (gap 4b): `Agent::with_profile` records the
    // resolved profile envelope but pre-fix the ToolContext built per call
    // always carried `ToolPermissions::default()` (allow-all). This test
    // proves the wired path: a profile that denies `shell` must produce a
    // ToolContext whose `permissions.is_tool_allowed("shell")` is false at
    // the actual call site.
    use octos_agent::profile::{PROFILE_SCHEMA_VERSION, ProfileDefinition, ProfileTools};

    let dir = TempDir::new().unwrap();
    let (probe, captured) = CtxProbeTool::new("perm_probe");

    let profile = Arc::new(ProfileDefinition {
        name: "no-shell".to_string(),
        version: PROFILE_SCHEMA_VERSION,
        tools: ProfileTools::DenyList {
            tools: vec!["shell".to_string()],
        },
        ..Default::default()
    });

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlm::new(vec![
        tool_use(vec![tc("call_1", "perm_probe", serde_json::json!({}))]),
        end("done"),
    ]));

    let agent = Agent::new(AgentId::new("m8-fix-perms"), llm, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_profile(profile);

    let _ = agent
        .process_message("invoke perm probe", &[], vec![])
        .await
        .expect("agent loop must succeed");

    let captured = captured
        .lock()
        .unwrap()
        .clone()
        .expect("probe must have observed a ToolContext");
    assert_eq!(
        captured.permissions_for.get("shell"),
        Some(&false),
        "profile deny-list for shell must reach the ToolContext: {:?}",
        captured.permissions_for
    );
    assert_eq!(
        captured.permissions_for.get("read_file"),
        Some(&true),
        "non-denied tools must remain permitted: {:?}",
        captured.permissions_for
    );
}
