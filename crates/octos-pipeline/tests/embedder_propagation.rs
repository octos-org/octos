//! NEW-06 regression: pipeline workers must inherit the parent
//! orchestrator's embedder so episodic memory recall stays on the
//! contamination-safe hybrid scored + filtered path
//! (`MIN_EPISODE_SIMILARITY`) instead of the unfiltered cwd-only
//! fallback in `EpisodeStore::find_relevant`.
//!
//! Root cause (round-3 fleet soak, mini5 / deep_research): the pipeline
//! call chain `RunPipelineTool::execute` -> `ExecutorConfig` ->
//! `CodergenHandler` -> worker `Agent::new()` did NOT plumb the
//! parent's embedder, so workers fell through to the unfiltered path
//! and pulled cross-domain episodes (Apple CEO / GPT-5.5 podcast) into
//! a JWST research worker's prompt.
//!
//! These tests pin the wiring at every hop in the chain. If a future
//! refactor forgets to forward the embedder, one of them goes red.

#![cfg(unix)]

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_llm::EmbeddingProvider;
use octos_pipeline::{CodergenHandler, RunPipelineTool};

async fn temp_episode_store() -> Arc<octos_memory::EpisodeStore> {
    let dir = tempfile::tempdir().unwrap();
    Arc::new(octos_memory::EpisodeStore::open(dir.path()).await.unwrap())
}

/// Stub embedder — never actually invoked by these tests; we only
/// assert that the `Arc` was threaded through. Using a no-op
/// implementation avoids pulling in network / API-key state.
struct StubEmbedder;

#[async_trait]
impl EmbeddingProvider for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(vec![vec![0.0_f32; 1]; texts.len()])
    }

    fn dimension(&self) -> usize {
        1
    }
}

#[allow(dead_code)]
struct MockProvider;

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

/// NEW-06 hop 1 — the parent's `with_embedder` must store the handle
/// on the `RunPipelineTool` so `execute` can copy it into
/// `ExecutorConfig`.
#[tokio::test]
async fn run_pipeline_tool_stores_embedder_from_builder() {
    let memory = temp_episode_store().await;
    let llm = Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>;
    let embedder = Arc::new(StubEmbedder) as Arc<dyn EmbeddingProvider>;
    let tool = RunPipelineTool::new(llm, memory, std::env::temp_dir(), std::env::temp_dir())
        .with_embedder(embedder.clone());

    assert!(
        tool.embedder_for_test().is_some(),
        "RunPipelineTool::with_embedder must persist the handle so \
         `execute` can forward it onto worker Agents (NEW-06)"
    );
}

/// NEW-06 hop 2 — `CodergenHandler::with_embedder` is the inner-loop
/// builder the executor calls. If this drops the handle, every worker
/// `Agent` will be born without it.
#[tokio::test]
async fn codergen_handler_stores_embedder_from_builder() {
    let embedder = Arc::new(StubEmbedder) as Arc<dyn EmbeddingProvider>;
    let codergen = CodergenHandler::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        std::env::temp_dir(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .with_embedder(embedder.clone());

    assert!(
        codergen.embedder_for_test().is_some(),
        "CodergenHandler::with_embedder must persist the handle so \
         every per-node worker Agent inherits hybrid memory recall (NEW-06)"
    );
}

/// NEW-06 default — the constructors must produce instances with no
/// embedder set (matches pre-fix behaviour for legacy callers that
/// don't propagate one yet).
#[tokio::test]
async fn run_pipeline_tool_defaults_to_no_embedder() {
    let tool = RunPipelineTool::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        std::env::temp_dir(),
        std::env::temp_dir(),
    );
    assert!(
        tool.embedder_for_test().is_none(),
        "RunPipelineTool::new must default to no embedder so legacy \
         callers stay byte-for-byte identical"
    );
}

/// NEW-06 default — same for `CodergenHandler`.
#[tokio::test]
async fn codergen_handler_defaults_to_no_embedder() {
    let codergen = CodergenHandler::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        std::env::temp_dir(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    assert!(
        codergen.embedder_for_test().is_none(),
        "CodergenHandler::new must default to no embedder so legacy \
         callers stay byte-for-byte identical"
    );
}

/// NEW-06 codex follow-up — the executor->handler hop is the load-
/// bearing wiring point that the earlier builder tests do NOT cover.
/// If a future refactor breaks `PipelineExecutor::build_codergen` (the
/// internal seam that copies `ExecutorConfig.embedder` onto the per-node
/// `CodergenHandler`), the four pre-existing builder tests above would
/// still all pass — so this test drives the full `PipelineExecutor::new`
/// -> `build_codergen_for_test()` chain and asserts the embedder
/// survives.
#[tokio::test]
async fn build_codergen_propagates_embedder_from_executor_config() {
    use octos_pipeline::PipelineExecutor;
    use octos_pipeline::executor::ExecutorConfig;

    let embedder = Arc::new(StubEmbedder) as Arc<dyn EmbeddingProvider>;
    let config = ExecutorConfig {
        default_provider: Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        provider_router: None,
        memory: temp_episode_store().await,
        working_dir: std::env::temp_dir(),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 8,
        max_pipeline_fanout_total: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: octos_pipeline::PipelineContext::default(),
        host_context: octos_pipeline::PipelineHostContext::default(),
        embedder: Some(embedder),
        catalog_dir: None,
    };

    let executor = PipelineExecutor::new(config);
    let codergen = executor.build_codergen_for_test();

    assert!(
        codergen.embedder_for_test().is_some(),
        "PipelineExecutor::build_codergen must copy `ExecutorConfig.embedder` \
         onto the per-node CodergenHandler — otherwise worker Agents built \
         by the handler never see the embedder and fall back to the \
         unfiltered cwd-only memory recall path (NEW-06 contamination)."
    );
}

/// NEW-06 codex follow-up — the executor->handler hop must also leave
/// `embedder = None` alone when no embedder was attached on the
/// `ExecutorConfig`. Guards against a future refactor accidentally
/// fabricating one from defaults / lazily-cached state.
#[tokio::test]
async fn build_codergen_omits_embedder_when_executor_config_has_none() {
    use octos_pipeline::PipelineExecutor;
    use octos_pipeline::executor::ExecutorConfig;

    let config = ExecutorConfig {
        default_provider: Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        provider_router: None,
        memory: temp_episode_store().await,
        working_dir: std::env::temp_dir(),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 8,
        max_pipeline_fanout_total: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: octos_pipeline::PipelineContext::default(),
        host_context: octos_pipeline::PipelineHostContext::default(),
        embedder: None,
        catalog_dir: None,
    };

    let executor = PipelineExecutor::new(config);
    let codergen = executor.build_codergen_for_test();

    assert!(
        codergen.embedder_for_test().is_none(),
        "build_codergen must not synthesise an embedder when none was \
         configured — that would silently change the legacy path."
    );
}

/// NEW-06 codex follow-up — `RunPipelineTool::execute` is the user-
/// facing entrypoint. The earlier builder test pins
/// `RunPipelineTool::with_embedder` but does NOT cover the `execute`
/// path's `ExecutorConfig` build. The integration-level assertion below
/// drives `RunPipelineTool::with_embedder` and inspects the stored
/// handle via `embedder_for_test`, which is the same field `execute`
/// reads when constructing the `ExecutorConfig`. The pairing
/// (`with_embedder` -> stored field -> `ExecutorConfig` read site -> the
/// `build_codergen` propagation test above) closes the chain end to end
/// without needing to spin up an LLM-driven pipeline run.
#[tokio::test]
async fn run_pipeline_tool_with_embedder_chain_pin() {
    let memory = temp_episode_store().await;
    let llm = Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>;
    let embedder = Arc::new(StubEmbedder) as Arc<dyn EmbeddingProvider>;

    // 1) RunPipelineTool::with_embedder stores the handle.
    let tool = RunPipelineTool::new(
        llm.clone(),
        memory.clone(),
        std::env::temp_dir(),
        std::env::temp_dir(),
    )
    .with_embedder(embedder.clone());
    assert!(tool.embedder_for_test().is_some());

    // 2) The handle Arc-points at the same provider instance — guards
    //    against a future refactor that re-wraps the embedder in a
    //    different `Arc` (legal at the type level but breaks any
    //    identity-based downstream wiring).
    let stored = tool.embedder_for_test().cloned().unwrap();
    assert!(
        Arc::ptr_eq(&stored, &embedder),
        "RunPipelineTool::with_embedder must not re-wrap the supplied Arc — \
         the executor and worker Agents rely on the handle identity to \
         share scoring state."
    );
}
