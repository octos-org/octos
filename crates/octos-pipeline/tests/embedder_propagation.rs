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
