use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use octos_llm::{ChatConfig, ChatResponse, ChatStream, LlmProvider, StopReason, ToolSpec};
use octos_memory::EpisodeStore;
use octos_pipeline::handler::HandlerContext;
use octos_pipeline::{CodergenHandler, Handler, HandlerKind, OutcomeStatus, PipelineNode};

struct HangingProvider;

#[async_trait]
impl LlmProvider for HangingProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        tokio::time::sleep(Duration::from_secs(3_600)).await;
        Ok(ChatResponse {
            content: Some("unexpected".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
            provider_index: None,
        })
    }

    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatStream> {
        tokio::time::sleep(Duration::from_secs(3_600)).await;
        Ok(Box::pin(futures::stream::empty()))
    }

    fn model_id(&self) -> &str {
        "hanging"
    }

    fn provider_name(&self) -> &str {
        "test"
    }
}

async fn episode_store(path: &Path) -> Arc<EpisodeStore> {
    Arc::new(EpisodeStore::open(path).await.expect("open episode store"))
}

#[tokio::test]
async fn codergen_timeout_secs_cancels_stalled_worker() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handler = CodergenHandler::new(
        Arc::new(HangingProvider),
        episode_store(dir.path()).await,
        dir.path().to_path_buf(),
        Arc::new(AtomicBool::new(false)),
    );
    let node = PipelineNode {
        id: "synthesize".to_string(),
        handler: HandlerKind::Codergen,
        prompt: Some("write the final report".to_string()),
        timeout_secs: Some(1),
        ..Default::default()
    };
    let ctx = HandlerContext {
        input: "research notes".to_string(),
        completed: HashMap::new(),
        predecessor_outcomes: vec![],
        working_dir: dir.path().to_path_buf(),
    };

    let started = Instant::now();
    let outcome = handler.execute(&node, &ctx).await.expect("node outcome");
    let elapsed = started.elapsed();

    assert_eq!(outcome.status, OutcomeStatus::Error);
    assert!(
        outcome.content.contains("timed out after 1s"),
        "unexpected outcome: {}",
        outcome.content
    );
    assert!(
        elapsed < Duration::from_millis(1_750),
        "worker timeout should trip promptly, got {elapsed:?}"
    );
}
