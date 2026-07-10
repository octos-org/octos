//! Regression test for the production wedge where a fan-out node whose child
//! task never resolves (its underlying work failed — e.g. `web_search` died on
//! a `deep_research` `search` node) hangs the WHOLE pipeline forever.
//!
//! Live symptom on the deployed box:
//!   * the heartbeat logged forever:
//!     `Pipeline 'deep_research' running: search (0/3 nodes, 1525s elapsed, …)`
//!     — 25+ minutes, ZERO of the fan-out children ever completing;
//!   * the server logged every 5s:
//!     `ignoring late mark_runtime_state: task already in terminal state …
//!      current_status="failed" … attempted_runtime_state=ExecutingTool`.
//!
//! Root cause: the fan-out worker futures (`execute_with_retries_static` at the
//! static `parallel` and `dynamic_parallel` sites in `executor.rs`) are
//! `join_all`-awaited with NO deadline guard — unlike the single-node
//! `dispatch_node` path which wraps execution in `tokio::time::timeout`. A
//! child whose future never resolves therefore wedges `join_all`, so the
//! fan-out node never converges, the pipeline never terminates, and every
//! attached client hangs indefinitely.
//!
//! These tests drive the real fan-out path with an injected handler whose
//! worker future hangs forever and assert the pipeline TERMINATES (with a
//! failure) in bounded wall-clock time.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use octos_core::TokenUsage;
use octos_memory::EpisodeStore;
use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor};
use octos_pipeline::graph::{HandlerKind, NodeOutcome, OutcomeStatus, PipelineNode};
use octos_pipeline::handler::{Handler, HandlerContext, HandlerRegistry};
use tempfile::TempDir;

// --- Stub provider: never called (the injected handler replaces dispatch). ---
struct StubProvider;

#[async_trait]
impl octos_llm::LlmProvider for StubProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some("stub".into()),
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            reasoning_content: None,
            provider_index: None,
        })
    }
    fn provider_name(&self) -> &str {
        "stub"
    }
    fn model_id(&self) -> &str {
        "stub-1"
    }
}

/// A handler that hangs forever for any node whose id is in `hang_nodes`, and
/// passes every other node. This reproduces the production child task that
/// reached a terminal `Failed` state but whose handler future never resolved —
/// leaving the fan-out `join_all` blocked and the pipeline wedged.
struct HangingWorkerHandler {
    /// Node ids whose `execute` future never resolves.
    hang_nodes: Vec<String>,
}

#[async_trait]
impl Handler for HangingWorkerHandler {
    async fn execute(
        &self,
        node: &PipelineNode,
        _ctx: &HandlerContext,
    ) -> eyre::Result<NodeOutcome> {
        if self.hang_nodes.iter().any(|n| n == &node.id) {
            // Never resolves — the smoking gun. In production this was a worker
            // Agent stuck after its web_search sub-task went terminal `Failed`,
            // still re-emitting `ExecutingTool` progress every 5s.
            std::future::pending::<()>().await;
            unreachable!("hanging worker future must never resolve");
        }
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: OutcomeStatus::Pass,
            content: format!("{} ok", node.id),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        })
    }
}

async fn make_executor(dir: &TempDir) -> PipelineExecutor {
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let config = ExecutorConfig {
        guards: Vec::new(),
        max_concurrent_llm_calls: None,
        default_provider: Arc::new(StubProvider) as Arc<dyn octos_llm::LlmProvider>,
        provider_router: None,
        memory,
        working_dir: dir.path().to_path_buf(),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 8,
        max_pipeline_fanout_total: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: octos_pipeline::context::PipelineContext::default(),
        host_context: octos_pipeline::host_context::PipelineHostContext::default(),
        embedder: None,
        catalog_dir: None,
        // #1607: pipeline validators run under a no-op sandbox in tests
        // (host-independent — command validators run the argv directly).
        sandbox: octos_agent::SandboxConfig::default(),
    };
    PipelineExecutor::new(config)
}

fn handlers_for(handler: Arc<HangingWorkerHandler>) -> HandlerRegistry {
    let mut handlers = HandlerRegistry::new();
    handlers.register(HandlerKind::Codergen, handler.clone());
    handlers.register(HandlerKind::Noop, handler.clone());
    handlers.register(HandlerKind::Parallel, handler.clone());
    handlers.register(HandlerKind::Gate, handler);
    handlers
}

/// RED: a `parallel` fan-out whose children ALL hang forever (the production
/// `search (0/3 nodes)` total-wedge) must TERMINATE the pipeline with a failure
/// in bounded time — it must NOT wedge `join_all` and report "running" forever.
#[tokio::test]
async fn parallel_fanout_all_children_hung_terminates_with_failure() {
    let dir = TempDir::new().unwrap();
    let exec = make_executor(&dir).await;

    // Both branches hang forever — exactly the deployed deep_research `search`
    // node where every fan-out child wedged (0/3 ever completing). Each worker
    // carries a short `timeout_secs` so the executor's per-worker deadline
    // guard cuts the hung branches off quickly. Before the fix there is NO such
    // guard, so `join_all` blocks and the outer test timeout fires.
    let dot = r#"digraph fanout {
        fan  [handler="parallel", converge="merge"]
        s1 [handler="codergen", tools=read_file, prompt="s1", timeout_secs="2"]
        s2 [handler="codergen", tools=read_file, prompt="s2", timeout_secs="2"]
        s3 [handler="codergen", tools=read_file, prompt="s3", timeout_secs="2"]
        merge [handler="codergen", tools=read_file, prompt="merge"]
        fan -> s1
        fan -> s2
        fan -> s3
        s1 -> merge
        s2 -> merge
        s3 -> merge
    }"#;

    let handler = Arc::new(HangingWorkerHandler {
        hang_nodes: vec!["s1".into(), "s2".into(), "s3".into()],
    });

    // Bound the WHOLE run so a regression manifests as a test timeout, not a
    // hung CI job. The fix must terminate well within this window.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        exec.run_with_handlers(
            dot,
            "user-input",
            &serde_json::Map::new(),
            handlers_for(handler),
        ),
    )
    .await;

    let result = result.expect(
        "pipeline wedged: a fan-out whose children never resolve hung the whole \
         pipeline (join_all blocked forever) — this is the production bug",
    );
    let result = result.expect("pipeline run returned an error result");

    assert!(
        !result.success,
        "a fan-out node whose every child failed must FAIL the pipeline, got success={}",
        result.success
    );
    // The hung children must never have fed the converge node: `merge` must NOT
    // have run on a usable-input path.
    assert!(
        result.output.contains("all 3 workers failed")
            || result.output.to_lowercase().contains("failed"),
        "expected an all-workers-failed failure message, got: {}",
        result.output
    );
}

/// A SINGLE hung child among otherwise-passing siblings must also TERMINATE in
/// bounded time (the hang is gone) — but here the fault-tolerant
/// "synthesize from what worked" path is preserved: the pipeline still
/// converges and succeeds because ≥1 worker passed. This pins that the
/// deadline fix did not turn every partial failure into a pipeline failure.
#[tokio::test]
async fn parallel_fanout_partial_failure_still_converges() {
    let dir = TempDir::new().unwrap();
    let exec = make_executor(&dir).await;

    let dot = r#"digraph fanout {
        fan  [handler="parallel", converge="merge"]
        good [handler="codergen", tools=read_file, prompt="good", timeout_secs="2"]
        bad  [handler="codergen", tools=read_file, prompt="bad",  timeout_secs="2"]
        merge [handler="codergen", tools=read_file, prompt="merge"]
        fan -> good
        fan -> bad
        good -> merge
        bad -> merge
    }"#;

    let handler = Arc::new(HangingWorkerHandler {
        hang_nodes: vec!["bad".into()],
    });

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        exec.run_with_handlers(
            dot,
            "user-input",
            &serde_json::Map::new(),
            handlers_for(handler),
        ),
    )
    .await
    .expect("pipeline wedged on a single hung child — the bound did not fire")
    .expect("pipeline run returned an error result");

    // ≥1 worker passed, so the fault-tolerant convergence runs and succeeds.
    assert!(
        result.success,
        "a partial fan-out failure (≥1 worker passed) must still converge and \
         succeed (fault tolerance preserved), got success={} output={}",
        result.success, result.output
    );
}
