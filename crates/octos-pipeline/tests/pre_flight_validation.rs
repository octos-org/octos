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
async fn pre_flight_rejects_dot_with_ambiguous_start() {
    // Real-world LLM mistake captured on mini5 2026-05-14: five parallel
    // search nodes without a common entry. Pre-fix, this only surfaced
    // inside the spawn_only background task — the LLM's foreground turn
    // had already returned "Pipeline started in background…" and never
    // saw the validator's complaint. Now the same shape is rejected
    // synchronously so the LLM can fix the DOT in the next iteration.
    let (tool, _working, _data) = make_tool().await;
    let args = serde_json::json!({
        "pipeline": "digraph bad {\n\
            search_a [handler=DynamicParallel, tools=search];\n\
            search_b [handler=DynamicParallel, tools=search];\n\
            search_c [handler=DynamicParallel, tools=search];\n\
            search_d [handler=DynamicParallel, tools=search];\n\
            search_e [handler=DynamicParallel, tools=search];\n\
            analyze [handler=Codergen, tools=read_file];\n\
            synthesize [handler=Codergen, tools=write_file];\n\
            search_a -> analyze;\n\
            search_b -> analyze;\n\
            search_c -> analyze;\n\
            search_d -> analyze;\n\
            search_e -> analyze;\n\
            analyze -> synthesize;\n\
        }",
        "input": "anything",
    });
    let err = tool
        .pre_flight_validate(&args)
        .await
        .expect_err("structurally invalid DOT must be rejected by pre-flight");
    assert!(
        err.contains("pipeline validation failed"),
        "error must surface the validator's complaint verbatim — got: {err}"
    );
    assert!(
        err.contains("ambiguous start") || err.contains("rule 1"),
        "error must identify rule 1 (ambiguous start) — got: {err}"
    );
}

#[tokio::test]
async fn pre_flight_accepts_well_formed_dot() {
    let (tool, _working, _data) = make_tool().await;
    let args = serde_json::json!({
        "pipeline": "digraph ok {\n\
            start [handler=Codergen, tools=read_file];\n\
            finish [handler=Codergen, tools=write_file];\n\
            start -> finish;\n\
        }",
        "input": "anything",
    });
    let result = tool.pre_flight_validate(&args).await;
    assert!(
        result.is_ok(),
        "well-formed DOT must pass pre-flight; got Err: {:?}",
        result.err()
    );
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
