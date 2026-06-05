//! Regression guard for `RunPipelineTool::pre_flight_validate` — surfaces
//! LLM-generated bad DOT as a synchronous foreground error so the LLM can
//! retry instead of leaking a spawn_only background failure with no
//! re-engagement path. See `crates/octos-agent/src/agent/execution.rs`
//! spawn_only intercept for the call site.

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

async fn make_tool() -> (RunPipelineTool, tempfile::TempDir, tempfile::TempDir) {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let memory_dir = data.path().join("episodes");
    let memory = Arc::new(octos_memory::EpisodeStore::open(&memory_dir).await.unwrap());
    let tool = RunPipelineTool::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        memory,
        PathBuf::from(working.path()),
        PathBuf::from(data.path()),
    );
    (tool, working, data)
}

#[tokio::test]
async fn pre_flight_rejects_inline_dot_even_when_well_formed() {
    // Killing the unsafe authoring surface: free-form inline DOT let a model
    // request arbitrary tools/handlers (incl. `shell`) or an empty tool-list
    // that silently expanded to all builtins. It is now rejected at the tool
    // boundary REGARDLESS of whether the DOT is structurally valid — validity
    // is irrelevant once the surface itself is unsafe. Agents author via the
    // capability-locked `ir` palette or name a sanctioned pipeline.
    let (tool, _working, _data) = make_tool().await;
    let args = serde_json::json!({
        "pipeline": "digraph ok {\n\
            start [handler=Codergen, tools=read_file];\n\
            finish [handler=Codergen, tools=write_file];\n\
            start -> finish;\n\
        }",
        "input": "anything",
    });
    let err = tool
        .pre_flight_validate(&args)
        .await
        .expect_err("inline DOT must be rejected even when well-formed");
    assert!(
        err.contains("inline DOT"),
        "error must name the inline-DOT rejection — got: {err}"
    );
    assert!(
        err.contains("`ir`") || err.contains("sanctioned pipeline"),
        "error must point the LLM at the safe alternatives — got: {err}"
    );
}

#[tokio::test]
async fn pre_flight_rejects_multi_node_inline_dot_before_structural_validation() {
    // Real-world LLM mistake captured on mini5 2026-05-14 (five parallel search
    // nodes without a common entry). It used to reach the structural validator
    // ("ambiguous start"); now the inline-DOT surface is rejected FIRST, so the
    // model never gets to author this shape at all. Structural-rule coverage
    // lives in `validate.rs` unit tests, exercised on named/bundled pipelines.
    let (tool, _working, _data) = make_tool().await;
    let args = serde_json::json!({
        "pipeline": "digraph bad {\n\
            search_a [handler=DynamicParallel, tools=search];\n\
            search_b [handler=DynamicParallel, tools=search];\n\
            analyze [handler=Codergen, tools=read_file];\n\
            search_a -> analyze;\n\
            search_b -> analyze;\n\
        }",
        "input": "anything",
    });
    let err = tool
        .pre_flight_validate(&args)
        .await
        .expect_err("inline DOT must be rejected by pre-flight");
    assert!(
        err.contains("inline DOT"),
        "rejection must fire before structural validation — got: {err}"
    );
}

#[tokio::test]
async fn resolve_rejects_inline_dot_at_chokepoint() {
    // The single resolution chokepoint both `execute` and `pre_flight` funnel
    // through must reject inline DOT, so no path can smuggle it to the parser.
    let (tool, _working, _data) = make_tool().await;
    let err = tool
        .resolve_named_for_test("digraph x { a -> b }")
        .await
        .expect_err("inline DOT must not resolve");
    assert!(
        err.to_string().contains("inline DOT"),
        "chokepoint must reject inline DOT — got: {err}"
    );
}

#[tokio::test]
async fn resolve_still_accepts_named_bundled_pipeline() {
    // Regression guard: killing inline DOT must NOT break the safe path. The
    // sanctioned bundled `deep_research` name still resolves to runnable DOT
    // (discovery miss → embedded bundled bytes).
    let (tool, _working, _data) = make_tool().await;
    let dot = tool
        .resolve_named_for_test("deep_research")
        .await
        .expect("bundled deep_research must still resolve");
    assert!(
        dot.contains("digraph"),
        "resolved named pipeline must be DOT — got: {dot:.80}"
    );
}

#[tokio::test]
async fn pre_flight_rejects_dot_file_paths() {
    // Security: a model-supplied `.dot` FILE PATH (e.g. one it wrote with
    // handler=shell) must be rejected — not read + executed via discovery's
    // direct-path resolution. Only bare sanctioned names are accepted.
    let (tool, _working, _data) = make_tool().await;
    for path in [
        "/tmp/pwn.dot",
        "./pwn.dot",
        "../x.dot",
        "subdir/p.dot",
        "pwn.dot",
    ] {
        let args = serde_json::json!({ "pipeline": path, "input": "x" });
        let err = tool
            .pre_flight_validate(&args)
            .await
            .expect_err("a .dot file path must be rejected");
        assert!(
            err.contains("file paths are not accepted"),
            "`{path}` must be rejected as a path; got: {err}"
        );
    }
}

#[tokio::test]
async fn agent_cannot_run_workspace_authored_pipeline_by_bare_name() {
    // Security: a model can write to the workspace; a `.dot` it drops in
    // `<working>/.octos/pipelines` must NOT be resolvable by bare name — the
    // agent tool searches only operator-trusted dirs + bundled bytes.
    let (tool, working, _data) = make_tool().await;
    let pdir = working.path().join(".octos").join("pipelines");
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::write(
        pdir.join("pwn.dot"),
        "digraph pwn { x [handler=shell, prompt=\"PWNED\"] }",
    )
    .unwrap();
    let err = tool
        .resolve_named_for_test("pwn")
        .await
        .expect_err("a workspace-authored pipeline must not resolve by bare name");
    assert!(
        !err.to_string().contains("PWNED"),
        "must NOT have read the workspace .dot; got: {err}"
    );
}

#[tokio::test]
async fn deep_research_resolves_to_the_bundled_ir() {
    // The sanctioned `deep_research` now ships as a capability-locked IR program
    // and runs the audited palette, not the embedded raw DOT.
    let (tool, _working, _data) = make_tool().await;
    assert_eq!(
        tool.resolve_named_kind_for_test("deep_research")
            .await
            .unwrap(),
        "ir",
        "deep_research must resolve to the bundled IR"
    );
    // ...and it pre-flights clean (the bundled IR composes under l2_default).
    let args = serde_json::json!({ "pipeline": "deep_research", "input": "x" });
    tool.pre_flight_validate(&args)
        .await
        .expect("bundled deep_research IR must pre-flight clean");
}

#[tokio::test]
async fn pre_flight_rejects_malformed_json_args() {
    let (tool, _working, _data) = make_tool().await;
    let args = serde_json::json!({ "pipeline": "digraph x { a; }" }); // missing required `input`
    let err = tool
        .pre_flight_validate(&args)
        .await
        .expect_err("missing required `input` must be rejected");
    assert!(
        err.contains("invalid run_pipeline input"),
        "error must reference the input shape — got: {err}"
    );
}
