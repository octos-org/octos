//! M8.6 concurrency-audit tests (item 6 of fix-first checklist).
//!
//! These tests pin the concurrency-class declarations for the
//! task-control / mutating tools the fix-first checklist called out:
//!
//! - `spawn` is `Exclusive` so a batch containing it serialises with
//!   any sibling.
//! - `check_background_tasks` is `Exclusive` so its supervisor
//!   snapshot is taken at a single point in batch order.
//! - Plugin and MCP wrappers can declare `Exclusive` instead of
//!   silently inheriting `Safe`.
//!
//! The tests query `Tool::concurrency_class()` directly. The
//! M8.8 scheduler integration that turns these into serial dispatch
//! is already covered by `concurrent_scheduler.rs` and
//! `m8_integration_tool_context.rs`.

use std::sync::Arc;

use async_trait::async_trait;
use octos_agent::{
    CheckBackgroundTasksTool, ConcurrencyClass, McpServerConfig, SpawnTool, Tool,
    plugins::PluginTool,
};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

#[tokio::test]
async fn mixed_batch_with_spawn_serializes() {
    // Item 6: spawn must report Exclusive so any batch containing it
    // is serialised by the M8.8 scheduler. This test does NOT need the
    // full agent loop — querying concurrency_class on the tool itself
    // is the load-bearing assertion, and the scheduler half of the
    // contract is covered by the dedicated concurrent_scheduler.rs
    // suite.
    let dir = TempDir::new().unwrap();
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel(8);
    let spawn_tool = SpawnTool::new(
        Arc::new(MockProvider::new()),
        memory,
        dir.path().to_path_buf(),
        inbound_tx,
    );
    assert_eq!(
        spawn_tool.concurrency_class(),
        ConcurrencyClass::Exclusive,
        "spawn() registers a background task with the supervisor and \
         must serialise against siblings — Safe default would race"
    );
}

#[test]
fn task_control_tools_are_exclusive() {
    // The task-control tool the checklist explicitly calls out:
    // check_background_tasks. The other names listed in the checklist
    // (send_to_agent, cancel_task, relaunch_task) are referenced in
    // the swarm profile but have no Tool implementation today; if/when
    // they land they will need their own concurrency_class override.
    let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
    let check = CheckBackgroundTasksTool::new(supervisor, "api:test");
    assert_eq!(
        check.concurrency_class(),
        ConcurrencyClass::Exclusive,
        "check_background_tasks reads supervisor state — must serialise \
         to take a coherent snapshot"
    );
}

#[test]
fn plugin_with_exclusive_manifest_serializes_with_other_exclusive_tools_in_batch() {
    // Build a PluginTool whose manifest declares
    // `concurrency_class: "exclusive"`. The wrapper must lift that
    // declaration into the trait method so the M8.8 scheduler sees
    // Exclusive and serialises the batch alongside other exclusive
    // tools (e.g. native `spawn`, `edit_file`).
    let exclusive_def = octos_agent::plugins::PluginToolDef {
        name: "exclusive_plugin".into(),
        description: "test".into(),
        input_schema: serde_json::json!({"type": "object"}),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: Some("exclusive".into()),
    };
    let exclusive_tool = PluginTool::new(
        "test-plugin".into(),
        exclusive_def,
        std::path::PathBuf::from("/bin/true"),
    );
    assert_eq!(
        exclusive_tool.concurrency_class(),
        ConcurrencyClass::Exclusive,
        "plugin manifest declared `exclusive` — wrapper must lift it"
    );
}

#[test]
fn plugin_with_no_concurrency_declaration_defaults_to_safe() {
    // Backward-compat: existing skills (without the new field) must
    // continue to register as Safe. Most bundled skills are read-only
    // (weather, news, time, deep-search) so Safe is the right default.
    let safe_def = octos_agent::plugins::PluginToolDef {
        name: "safe_plugin".into(),
        description: "test".into(),
        input_schema: serde_json::json!({"type": "object"}),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let safe_tool = PluginTool::new(
        "test-plugin".into(),
        safe_def,
        std::path::PathBuf::from("/bin/true"),
    );
    assert_eq!(
        safe_tool.concurrency_class(),
        ConcurrencyClass::Safe,
        "plugin manifest with no hint must fall back to Safe — the \
         legacy default the fix-first checklist preserves"
    );
}

#[test]
fn mcp_wrapper_defaults_to_safe_when_metadata_absent() {
    // M8 req 10: legacy operator configs that pre-date the
    // `concurrency_class` field must keep parsing. The resolved class
    // falls through to `Safe` because most MCP servers in practice
    // are read-only (search, wiki, time, weather) and forcing
    // Exclusive on all of them would serialise the fast common path.
    // Operators who run mutating MCP servers declare `"exclusive"`
    // explicitly — see `mcp_wrapper_exclusive_opt_in_propagates`.
    let config: McpServerConfig = serde_json::from_str(r#"{"command": "/bin/true"}"#).unwrap();
    assert!(
        config.concurrency_class.is_none(),
        "field is optional and absent in legacy configs"
    );
    assert_eq!(
        config.resolved_concurrency_class(),
        ConcurrencyClass::Safe,
        "MCP defaults to Safe for the read-only common case"
    );
}

#[test]
fn existing_app_skills_continue_to_register_after_field_addition() {
    // M8 req 10 backward-compat gate: bundled skills shipped with
    // older `manifest.json` files (no `concurrency_class` key on
    // either tool defs or mcpServers entries) must keep parsing and
    // registering exactly as before. This pins the JSON-shape
    // contract for both wrappers.
    let legacy_plugin: octos_agent::plugins::PluginToolDef = serde_json::from_str(
        r#"{"name": "weather", "description": "lookup", "input_schema": {"type": "object"}}"#,
    )
    .unwrap();
    assert!(legacy_plugin.concurrency_class.is_none());

    let legacy_mcp: McpServerConfig =
        serde_json::from_str(r#"{"command": "/bin/true", "args": ["--mcp"]}"#).unwrap();
    assert!(legacy_mcp.concurrency_class.is_none());
    assert_eq!(
        legacy_mcp.resolved_concurrency_class(),
        ConcurrencyClass::Safe,
        "legacy configs resolve to Safe — read-only common case"
    );
}

#[test]
fn mcp_wrapper_exclusive_opt_in_propagates() {
    // Operators who run a mutating MCP server (one that writes files
    // into the workspace, posts to a remote service, etc.) declare
    // `"exclusive"` so the M8.8 scheduler serialises the batch and
    // prevents racing with the native `edit_file` / `write_file`
    // tools.
    let config: McpServerConfig =
        serde_json::from_str(r#"{"command": "/bin/true", "concurrency_class": "exclusive"}"#)
            .unwrap();
    assert_eq!(
        config.resolved_concurrency_class(),
        ConcurrencyClass::Exclusive,
        "operator-declared `exclusive` lifts to Exclusive enforcement"
    );
}

#[test]
fn mcp_unknown_concurrency_value_falls_back_to_exclusive() {
    // Typos must not silently downgrade enforcement. Unknown values
    // resolve to the safe-side default.
    let config: McpServerConfig =
        serde_json::from_str(r#"{"command": "/bin/true", "concurrency_class": "exlusive"}"#)
            .unwrap();
    assert_eq!(
        config.resolved_concurrency_class(),
        ConcurrencyClass::Exclusive,
        "typo'd values must fall back to Exclusive — fail-safe"
    );
}

// ---------------------------------------------------------------------------
// Helpers shared by the spawn-tool concurrency test.
// ---------------------------------------------------------------------------

struct MockProvider;

impl MockProvider {
    fn new() -> Self {
        Self
    }
}

#[async_trait]
impl octos_llm::LlmProvider for MockProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some("ok".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }
    fn context_window(&self) -> u32 {
        128_000
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}
