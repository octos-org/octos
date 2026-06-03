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
