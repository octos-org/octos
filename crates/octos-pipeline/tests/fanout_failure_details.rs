//! A failed research fan-out must explain the provider failure to its caller.
use std::sync::Arc;

use async_trait::async_trait;
use octos_core::TokenUsage;
use octos_memory::EpisodeStore;
use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor};
use octos_pipeline::graph::{HandlerKind, NodeOutcome, OutcomeStatus, PipelineNode};
use octos_pipeline::handler::{Handler, HandlerContext, HandlerRegistry};

struct Planner;

#[async_trait]
impl octos_llm::LlmProvider for Planner {
    async fn chat(
        &self,
        _: &[octos_core::Message],
        _: &[octos_llm::ToolSpec],
        _: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some(
                r#"[{"task":"first","label":"first"},{"task":"second","label":"second"}]"#.into(),
            ),
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: Default::default(),
            reasoning_content: None,
            provider_index: None,
        })
    }
    fn provider_name(&self) -> &str {
        "test"
    }
    fn model_id(&self) -> &str {
        "planner"
    }
}

struct RejectedWorker {
    returns_error: bool,
}

#[async_trait]
impl Handler for RejectedWorker {
    async fn execute(&self, node: &PipelineNode, _: &HandlerContext) -> eyre::Result<NodeOutcome> {
        assert_ne!(
            node.id, "merge",
            "all-failed workers must not reach synthesis"
        );
        let error = format!(
            "API error (deepseek): authentication failed — HTTP 401; api_key=sk-test-research-secret-1234567890 {}",
            "详情".repeat(10_000)
        );
        if self.returns_error {
            eyre::bail!(error);
        }
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: OutcomeStatus::Error,
            content: error,
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        })
    }
}

async fn check_failure_details(dynamic: bool, returns_error: bool) {
    let dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let exec = PipelineExecutor::new(ExecutorConfig {
        default_provider: Arc::new(Planner),
        memory,
        working_dir: dir.path().to_path_buf(),
        guards: vec![],
        max_concurrent_llm_calls: None,
        provider_router: None,
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 8,
        max_pipeline_fanout_total: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: Default::default(),
        host_context: Default::default(),
        embedder: None,
        catalog_dir: None,
        sandbox: Default::default(),
    });
    let dot = if dynamic {
        r#"digraph research {
            search [handler="dynamic_parallel", converge="merge", model="cheap", max_tasks="2", tools="read_file", prompt="plan", worker_prompt="{task}"]
            merge [prompt="merge", tools="read_file"]
            search -> merge
        }"#
    } else {
        r#"digraph research {
            search [handler="parallel", converge="merge"]
            first [prompt="first", model="cheap", tools="read_file"]
            second [prompt="second", model="cheap", tools="read_file"]
            merge [prompt="merge", tools="read_file"]
            search -> first
            search -> second
            first -> merge
            second -> merge
        }"#
    };
    let mut handlers = HandlerRegistry::new();
    let handler = Arc::new(RejectedWorker { returns_error });
    handlers.register(HandlerKind::Codergen, handler.clone());
    handlers.register(HandlerKind::Parallel, handler.clone());
    handlers.register(HandlerKind::DynamicParallel, handler);
    let result = exec
        .run_with_handlers(dot, "test", &serde_json::Map::new(), handlers)
        .await
        .unwrap();
    assert!(!result.success);
    assert!(
        result.output.contains("all 2 workers failed"),
        "{}",
        result.output
    );
    assert!(
        result.output.contains("HTTP 401"),
        "provider cause was discarded: {}",
        result.output
    );
    assert!(result.output.contains("deepseek"));
    assert!(result.output.contains("cheap"), "missing model lane");
    assert!(
        result
            .output
            .contains(if dynamic { "search_task_0" } else { "first" })
    );
    assert!(!result.output.contains("sk-test-research-secret-1234567890"));
    assert!(
        result.output.len() < 8192,
        "failure diagnostics must be bounded"
    );
    assert_eq!(result.token_usage.input_tokens, 0);
    assert_eq!(result.token_usage.output_tokens, 0);
}

#[tokio::test]
async fn static_fanout_preserves_worker_error_outcomes() {
    check_failure_details(false, false).await;
}

#[tokio::test]
async fn static_fanout_preserves_worker_handler_errors() {
    check_failure_details(false, true).await;
}

#[tokio::test]
async fn dynamic_fanout_preserves_worker_error_outcomes() {
    check_failure_details(true, false).await;
}

#[tokio::test]
async fn dynamic_fanout_preserves_worker_handler_errors() {
    check_failure_details(true, true).await;
}
