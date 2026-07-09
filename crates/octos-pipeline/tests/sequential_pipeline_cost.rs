use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_agent::cost_ledger::{CostAccountant, CostLedger, PersistentCostLedger};
use octos_core::TokenUsage;
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage as LlmTokenUsage};
use octos_memory::EpisodeStore;
use octos_pipeline::context::PipelineContext;
use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor};
use octos_pipeline::handler::HandlerContext;
use octos_pipeline::{
    Handler, HandlerKind, HandlerRegistry, NodeOutcome, NoopHandler, OutcomeStatus, PipelineNode,
};
use tokio::time::timeout;

const CONTRACT_ID: &str = "same-session-pipeline-cost";
const TWO_NODE_DOT: &str = r#"
digraph aggregate_cost {
    draft [handler="codergen", model="claude-haiku", tools=""]
    refine [handler="codergen", model="claude-haiku", tools=""]
    draft -> refine
}
"#;

struct TokenHandler {
    invocations: AtomicUsize,
}

impl TokenHandler {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            invocations: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Handler for TokenHandler {
    async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
        let call = self.invocations.fetch_add(1, Ordering::Relaxed) as u32 + 1;
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: OutcomeStatus::Pass,
            content: format!("{} complete", node.id),
            token_usage: TokenUsage {
                input_tokens: 1_000 * call,
                output_tokens: 500 * call,
                ..Default::default()
            },
            files_modified: vec![],
        })
    }
}

struct MockProvider;

#[async_trait]
impl LlmProvider for MockProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        Ok(ChatResponse {
            content: Some("ok".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: LlmTokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

async fn episode_store(path: &std::path::Path) -> Arc<EpisodeStore> {
    Arc::new(EpisodeStore::open(path).await.expect("open episode store"))
}

fn config(
    working_dir: PathBuf,
    memory: Arc<EpisodeStore>,
    accountant: Arc<CostAccountant>,
) -> ExecutorConfig {
    ExecutorConfig {
        guards: Vec::new(),
        max_concurrent_llm_calls: None,
        default_provider: Arc::new(MockProvider) as Arc<dyn LlmProvider>,
        provider_router: None,
        memory,
        working_dir,
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(AtomicBool::new(false)),
        max_parallel_workers: 1,
        max_pipeline_fanout_total: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: PipelineContext::new()
            .with_cost_accountant(accountant)
            .with_contract_id(CONTRACT_ID)
            .with_projected_usd(0.01),
        host_context: octos_pipeline::host_context::PipelineHostContext::default(),
        embedder: None,
        catalog_dir: None,
    }
}

fn handlers(token_handler: Arc<TokenHandler>) -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    registry.register(HandlerKind::Codergen, token_handler);
    registry.register(HandlerKind::Noop, Arc::new(NoopHandler));
    registry.register(HandlerKind::Shell, Arc::new(NoopHandler));
    registry.register(HandlerKind::Gate, Arc::new(NoopHandler));
    registry.register(HandlerKind::Parallel, Arc::new(NoopHandler));
    registry.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));
    registry
}

#[tokio::test]
async fn sequential_pipelines_commit_nonzero_aggregate_cost() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = Arc::new(PersistentCostLedger::open(dir.path()).await.unwrap());
    let accountant = Arc::new(CostAccountant::new(ledger.clone(), None));
    let token_handler = TokenHandler::new();
    let executor = PipelineExecutor::new(config(
        dir.path().to_path_buf(),
        episode_store(dir.path()).await,
        accountant,
    ));
    let variables = serde_json::Map::new();

    let first = timeout(
        Duration::from_secs(5),
        executor.run_with_handlers(
            TWO_NODE_DOT,
            "first prompt",
            &variables,
            handlers(token_handler.clone()),
        ),
    )
    .await
    .expect("first pipeline must not stall")
    .expect("first pipeline succeeds");
    let second = timeout(
        Duration::from_secs(5),
        executor.run_with_handlers(
            TWO_NODE_DOT,
            "second prompt",
            &variables,
            handlers(token_handler),
        ),
    )
    .await
    .expect("second same-session pipeline must not stall")
    .expect("second pipeline succeeds");

    assert!(first.success, "first pipeline failed: {}", first.output);
    assert!(second.success, "second pipeline failed: {}", second.output);
    assert_eq!(first.node_costs.len(), 2, "one cost row per first-run node");
    assert_eq!(
        second.node_costs.len(),
        2,
        "one cost row per second-run node"
    );

    let expected_total: f64 = first
        .node_costs
        .iter()
        .chain(second.node_costs.iter())
        .map(|row| {
            assert!(
                row.actual_usd > 0.0,
                "node {} must carry non-zero cost: {row:?}",
                row.node_id
            );
            row.actual_usd
        })
        .sum();

    let rows = ledger.list_for_contract(CONTRACT_ID).await.unwrap();
    assert_eq!(
        rows.len(),
        2,
        "each successful pipeline run must commit one aggregate ledger row"
    );
    assert!(
        rows.iter().all(|row| row.cost_usd > 0.0),
        "aggregate rows must be non-zero: {rows:?}"
    );
    let ledger_total: f64 = rows.iter().map(|row| row.cost_usd).sum();
    assert!(
        (ledger_total - expected_total).abs() < 1e-12,
        "ledger total {ledger_total} must equal per-node total {expected_total}"
    );

    let rollups = ledger.aggregate_per_contract().await.unwrap();
    let rollup = rollups
        .iter()
        .find(|rollup| rollup.contract_id == CONTRACT_ID)
        .expect("rollup for shared contract");
    assert_eq!(rollup.dispatch_count, 2);
    assert!(
        (rollup.cost_usd - expected_total).abs() < 1e-12,
        "rollup total {} must equal per-node total {expected_total}",
        rollup.cost_usd
    );
}
