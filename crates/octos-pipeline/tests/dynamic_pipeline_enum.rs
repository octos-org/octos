//! Gap 4.1 — the `run_pipeline` tool must advertise a `pipeline` enum that
//! reflects the LIVE discovery list, not a hard-coded `["deep_research"]`.
//!
//! Live mini5 soak failure: `run_pipeline deep_research` returned
//! `Available: (none)` because the `mofa-research` skill carrying
//! `deep_research.dot` had drifted off the profile. Bundling the `.dot`
//! (octos-agent) plus making the advertised enum match reality (here) keeps
//! the model from emitting names that don't exist — and surfaces the names
//! that DO exist.

use std::path::PathBuf;
use std::sync::Arc;

use octos_agent::Tool;
use octos_pipeline::RunPipelineTool;

struct MockProvider;

#[async_trait::async_trait]
impl octos_llm::LlmProvider for MockProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some("ok".into()),
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            reasoning_content: None,
            provider_index: None,
        })
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
    fn model_id(&self) -> &str {
        "mock-1"
    }
}

async fn make_tool_with_data(working: &std::path::Path, data: &std::path::Path) -> RunPipelineTool {
    let memory_dir = data.join("episodes");
    let memory = Arc::new(octos_memory::EpisodeStore::open(&memory_dir).await.unwrap());
    RunPipelineTool::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        memory,
        PathBuf::from(working),
        PathBuf::from(data),
    )
}

fn enum_values(schema: &serde_json::Value) -> Vec<String> {
    schema["properties"]["pipeline"]["enum"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// RED today: the enum is hard-coded to `["deep_research"]`, so a discovered
/// pipeline with a DIFFERENT name never shows up.
#[tokio::test]
async fn pipeline_enum_reflects_discovered_pipelines() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    // Install a pipeline with a name that is NOT the legacy hard-coded one.
    let pipelines_dir = data.path().join("pipelines");
    std::fs::create_dir_all(&pipelines_dir).unwrap();
    std::fs::write(
        pipelines_dir.join("custom_flow.dot"),
        "digraph custom_flow { a [prompt=\"hi\"] }",
    )
    .unwrap();

    let tool = make_tool_with_data(working.path(), data.path()).await;
    let schema = tool.input_schema();
    let values = enum_values(&schema);

    assert!(
        values.iter().any(|v| v == "custom_flow"),
        "advertised pipeline enum must include the discovered 'custom_flow' \
         pipeline, got {values:?}"
    );
}

/// When no pipelines can be discovered, the schema must NOT crash and must
/// fall back sensibly (keep the `deep_research` baseline name advertised so
/// the model still has the sanctioned generic pipeline available, matching
/// the bundled fallback).
#[tokio::test]
async fn pipeline_enum_falls_back_when_no_discovery() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // Intentionally no pipelines dir / no .dot files anywhere.

    let tool = make_tool_with_data(working.path(), data.path()).await;
    let schema = tool.input_schema();
    let values = enum_values(&schema);

    assert!(
        !values.is_empty(),
        "empty discovery must still advertise a non-empty fallback enum"
    );
    assert!(
        values.iter().any(|v| v == "deep_research"),
        "no-discovery fallback must advertise the baseline 'deep_research' name, got {values:?}"
    );
}

/// Gap 4.1 NIT 2 — the fallback enum must NOT advertise a pipeline the tool
/// cannot actually resolve. RED before the fix: with NO discovery (no
/// bootstrap, empty dirs) the enum advertised `deep_research` (via the
/// unconditional fallback), but `pre_flight_validate("deep_research")`
/// FAILED with `Available: (none)` — a masking lie. After: the named
/// resolver falls back to the embedded bundled bytes for the sanctioned
/// `deep_research`, so advertise == resolvable on every path, including a
/// degraded filesystem where bootstrap's disk write failed.
#[tokio::test]
async fn fallback_advertised_deep_research_actually_resolves() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // Intentionally no pipelines dir, no bootstrap, no .dot files anywhere.

    let tool = make_tool_with_data(working.path(), data.path()).await;

    // (a) advertised
    let values = enum_values(&tool.input_schema());
    assert!(
        values.iter().any(|v| v == "deep_research"),
        "fallback must advertise deep_research, got {values:?}"
    );

    // (b) MUST actually resolve (this is the masking guard).
    let args = serde_json::json!({ "pipeline": "deep_research", "input": "x" });
    tool.pre_flight_validate(&args).await.expect(
        "an advertised pipeline MUST be resolvable — the enum fallback must not \
         advertise a name the tool cannot resolve (NIT 2 masking guard)",
    );
}

/// `with_octos_home` must add `<octos_home>/pipelines` as a discovery search
/// path so bundled pipelines written there by
/// `bootstrap_bundled_pipelines` are advertised in the enum.
#[tokio::test]
async fn pipeline_enum_includes_octos_home_pipelines() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let octos_home = tempfile::tempdir().unwrap();

    // Bundled pipeline lands in <octos_home>/pipelines (per bootstrap).
    // Use a NON-baseline name so a hard-coded fallback can't satisfy this.
    let home_pipelines = octos_home.path().join("pipelines");
    std::fs::create_dir_all(&home_pipelines).unwrap();
    std::fs::write(
        home_pipelines.join("home_bundled_flow.dot"),
        "digraph home_bundled_flow { a [prompt=\"hi\"] }",
    )
    .unwrap();

    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_octos_home(PathBuf::from(octos_home.path()));
    let schema = tool.input_schema();
    let values = enum_values(&schema);

    assert!(
        values.iter().any(|v| v == "home_bundled_flow"),
        "with_octos_home must surface <octos_home>/pipelines/home_bundled_flow.dot \
         in the advertised enum, got {values:?}"
    );
}

/// Collect every tool NAME the host actually registers for a pipeline
/// worker: the built-in tool registry (`ToolRegistry::with_builtins`)
/// PLUS every tool exported by the bundled app-skill / platform-skill
/// manifests (these are loaded into the worker registry via
/// `plugin_dirs`, see `handler.rs::CodergenHandler::execute`). A bundled
/// `.dot` may only reference / allow-list names in this set — otherwise
/// the worker's allow-list policy (handler.rs) silently drops the tool
/// and the node cannot do its job at runtime.
fn registered_tool_names() -> std::collections::HashSet<String> {
    let mut names: std::collections::HashSet<String> =
        octos_agent::ToolRegistry::with_builtins(std::env::temp_dir())
            .tool_names()
            .into_iter()
            .collect();

    // Bundled plugin (app-skill + platform-skill) tools the pipeline
    // worker loads via plugin_dirs. Parse each manifest's `tools[].name`.
    let manifests = octos_agent::bundled_app_skills::BUNDLED_APP_SKILLS
        .iter()
        .chain(octos_agent::bundled_app_skills::PLATFORM_SKILLS.iter())
        .map(|&(_, _, _, manifest_json)| manifest_json);
    for manifest_json in manifests {
        let manifest: serde_json::Value =
            serde_json::from_str(manifest_json).expect("bundled manifest must be valid JSON");
        if let Some(tools) = manifest.get("tools").and_then(|t| t.as_array()) {
            for tool in tools {
                if let Some(name) = tool.get("name").and_then(|n| n.as_str()) {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

/// Collect every tool reference a bundled `.dot` carries: the union of
/// every node's `tools=` allow-list across all nodes. This is the set
/// the handler turns into a `ToolPolicy.allow` list — anything not
/// registered is unreachable at runtime.
fn dot_tool_references(dot: &str) -> std::collections::BTreeSet<String> {
    let graph = octos_pipeline::parser::parse_dot(dot).expect("bundled .dot must parse");
    let mut refs = std::collections::BTreeSet::new();
    for node in graph.nodes.values() {
        for tool in &node.tools {
            let t = tool.trim();
            if !t.is_empty() {
                refs.insert(t.to_string());
            }
        }
    }
    refs
}

/// Gap 4.1 BLOCKER 1 — the missing test class. Every tool a bundled
/// `.dot` references (its `tools=` allow-list) MUST resolve to a tool
/// the host actually registers. RED on e31665ca: `deep_research.dot`
/// allow-listed `deep_search`, but octos registers that tool as `search`
/// (the in-process `DeepSearchTool` names itself `search`; the
/// deep-search app-skill manifest exports `search`). The pipeline worker
/// applies the DOT allow-list (handler.rs), so `deep_search` was unknown
/// → the node could never run the web search it was built for.
#[test]
fn every_bundled_dot_tool_reference_is_registered() {
    let registered = registered_tool_names();
    assert!(
        registered.contains("search"),
        "precondition: the registered set must include the `search` tool \
         (DeepSearchTool / deep-search app-skill), got {} names",
        registered.len()
    );

    for &(file_name, dot) in octos_agent::bundled_pipelines::BUNDLED_PIPELINES {
        let refs = dot_tool_references(dot);
        let unregistered: Vec<&String> = refs.iter().filter(|r| !registered.contains(*r)).collect();
        assert!(
            unregistered.is_empty(),
            "bundled pipeline '{file_name}' references tool(s) that octos does NOT \
             register: {unregistered:?} (every `tools=` entry must be a name the \
             host registers — builtins or a bundled-skill manifest export). \
             All references: {refs:?}"
        );
    }
}

/// Gap 4.1 BLOCKER 2 (chat/serve path) — those hosts bootstrap the bundle
/// into `<data_dir>/bundled-pipelines` and register it via
/// `with_bundled_pipelines_root(data_dir)` (NOT `with_octos_home`). The
/// invariant: the dir bootstrap writes into is exactly the dir discovery
/// searches. RED before the fix: chat wrote the bundle to
/// `<data_dir>/pipelines` (which also shadowed installs) and never
/// registered a dedicated bundled dir.
#[tokio::test]
async fn chat_path_bootstrap_dir_equals_search_dir() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    // chat.rs bootstraps the bundle into `data_dir`.
    let written = octos_agent::bootstrap::bootstrap_bundled_pipelines(data.path());
    assert!(written >= 1, "bootstrap must write at least deep_research");

    // chat.rs builds the tool with `with_bundled_pipelines_root(data_dir)`.
    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_bundled_pipelines_root(PathBuf::from(data.path()));

    let values = enum_values(&tool.input_schema());
    assert!(
        values.iter().any(|v| v == "deep_research"),
        "chat path: bootstrapped deep_research must be discoverable, got {values:?}"
    );
    let args = serde_json::json!({ "pipeline": "deep_research", "input": "x" });
    tool.pre_flight_validate(&args)
        .await
        .expect("chat path: bootstrapped deep_research must resolve + validate");
}

/// Gap 4.1 BLOCKER 2 + 3 (gateway path) — the gateway bootstraps into
/// `<effective_octos_home>/bundled-pipelines` and registers discovery via
/// `with_octos_home(effective_octos_home)`. An installed skill copy in
/// `<octos_home>/skills/<x>/deep_research.dot` must WIN over the bundled
/// one, AND the bundled one must still resolve when no install exists.
#[tokio::test]
async fn gateway_path_installed_wins_and_bundled_discovers() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let octos_home = tempfile::tempdir().unwrap();

    octos_agent::bootstrap::bootstrap_bundled_pipelines(octos_home.path());

    // No install yet: bundled must resolve.
    {
        let tool = make_tool_with_data(working.path(), data.path())
            .await
            .with_octos_home(PathBuf::from(octos_home.path()));
        let dot = tool
            .resolve_named_for_test("deep_research")
            .await
            .expect("bundled deep_research must resolve via with_octos_home");
        assert!(
            dot.contains("digraph deep_research"),
            "bundled copy must resolve when no install exists"
        );
        // The fixed tool name must be present (regression guard for Blocker 1).
        assert!(
            dot.contains("tools=\"search,read_file\""),
            "bundled deep_research must allow-list the registered `search` tool, not `deep_search`"
        );
    }

    // Install a skill copy of the same name — it must now win.
    let skill_dir = octos_home.path().join("skills").join("mofa-research");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("deep_research.dot"),
        "digraph deep_research { installed [prompt=\"INSTALLED\"] }",
    )
    .unwrap();

    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_octos_home(PathBuf::from(octos_home.path()));
    let dot = tool
        .resolve_named_for_test("deep_research")
        .await
        .expect("installed deep_research must resolve");
    assert!(
        dot.contains("INSTALLED"),
        "installed skill deep_research.dot must win over the bundled copy, got: {dot}"
    );
}

/// Cross-crate guard: every pipeline bundled by `octos_agent` must parse and
/// validate clean against THIS crate's parser/validator — otherwise
/// `pre_flight_validate` would reject the bundled fallback the moment the
/// model named it.
#[test]
fn bundled_pipelines_parse_and_validate_clean() {
    for &(file_name, dot) in octos_agent::bundled_pipelines::BUNDLED_PIPELINES {
        let graph = octos_pipeline::parser::parse_dot(dot)
            .unwrap_or_else(|e| panic!("bundled pipeline '{file_name}' fails to parse: {e}"));
        let diags = octos_pipeline::validate::validate(&graph);
        assert!(
            !octos_pipeline::validate::has_errors(&diags),
            "bundled pipeline '{file_name}' has validation errors: {:?}",
            diags
                .iter()
                .filter(|d| d.severity == octos_pipeline::validate::Severity::Error)
                .collect::<Vec<_>>()
        );
    }
}

/// End-to-end: after `bootstrap_bundled_pipelines` writes into
/// `<octos_home>/pipelines`, a `RunPipelineTool` built with `with_octos_home`
/// advertises `deep_research` AND can `resolve` it. This is the exact path
/// the mini5 soak missed (skill drift → `Available: (none)`).
#[tokio::test]
async fn bootstrap_then_discover_deep_research_end_to_end() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let octos_home = tempfile::tempdir().unwrap();

    let written = octos_agent::bootstrap::bootstrap_bundled_pipelines(octos_home.path());
    assert!(written >= 1, "bootstrap must write at least deep_research");

    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_octos_home(PathBuf::from(octos_home.path()));

    let values = enum_values(&tool.input_schema());
    assert!(
        values.iter().any(|v| v == "deep_research"),
        "bootstrapped deep_research must be advertised, got {values:?}"
    );

    // And it must actually resolve (pre_flight_validate's resolve step).
    let args = serde_json::json!({ "pipeline": "deep_research", "input": "x" });
    tool.pre_flight_validate(&args)
        .await
        .expect("bootstrapped deep_research must pass pre_flight_validate");
}
