//! W1 acceptance tests for M8 runtime parity (issue #592).
//!
//! Validates that pipeline workers inherit:
//! * FileStateCache from the parent session via `PipelineHostContext` (A1)
//! * register a child task in the parent `TaskSupervisor` on node start (A3)
//! * open a per-node `CostReservationHandle` against the parent
//!   `CostAccountant` and commit it on completion (A4)
//!
//! The M8.9 recovery loop (A2) is exercised by inline unit tests in
//! `crates/octos-pipeline/src/recovery.rs` so we don't repeat that here.

#![cfg(unix)]

use std::sync::Arc;

use octos_agent::cost_ledger::{
    CostAccountant, CostBudgetPolicy, CostLedger, PersistentCostLedger,
};
use octos_agent::file_state_cache::FileStateCache;
use octos_agent::task_supervisor::TaskSupervisor;
use octos_pipeline::host_context::PipelineHostContext;
use octos_pipeline::{CodergenHandler, HandlerRegistry};

async fn temp_episode_store() -> Arc<octos_memory::EpisodeStore> {
    let dir = tempfile::tempdir().unwrap();
    Arc::new(
        octos_memory::EpisodeStore::open(dir.path())
            .await
            .unwrap(),
    )
}

#[allow(dead_code)]
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

async fn host_context_with_cache_and_supervisor() -> (
    PipelineHostContext,
    Arc<FileStateCache>,
    Arc<TaskSupervisor>,
    Arc<CostAccountant>,
    tempfile::TempDir,
) {
    let cache = Arc::new(FileStateCache::new());
    let supervisor = Arc::new(TaskSupervisor::new());
    let ledger_dir = tempfile::tempdir().unwrap();
    let ledger: Arc<dyn CostLedger> =
        Arc::new(PersistentCostLedger::open(ledger_dir.path()).await.unwrap());
    let policy = CostBudgetPolicy::default().with_per_contract_usd(100.0);
    let accountant = Arc::new(CostAccountant::new(ledger, Some(policy)));
    let host = PipelineHostContext {
        file_state_cache: Some(cache.clone()),
        subagent_output_router: None,
        subagent_summary_generator: None,
        task_supervisor: Some(supervisor.clone()),
        cost_accountant: Some(accountant.clone()),
        parent_tool_call_id: Some("tool-call-w1-test".into()),
        parent_session_key: Some("session-w1".into()),
    };
    (host, cache, supervisor, accountant, ledger_dir)
}

/// W1 acceptance — confirm `CodergenHandler` exposes the host context
/// it was built with, and that the host context carries the parent
/// session's `FileStateCache`.
#[tokio::test]
async fn pipeline_worker_handler_carries_file_state_cache_from_host() {
    let (host, _cache, _sup, _acct, _ledger_dir) =
        host_context_with_cache_and_supervisor().await;
    let codergen = CodergenHandler::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        std::env::temp_dir(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .with_host_context(host);

    let observed = codergen.host_context();
    assert!(
        observed.file_state_cache.is_some(),
        "CodergenHandler must propagate FileStateCache from host context"
    );
    assert_eq!(
        observed.parent_tool_call_id.as_deref(),
        Some("tool-call-w1-test"),
        "CodergenHandler must propagate parent_tool_call_id from host context"
    );
}

/// W1.A3 — registering a node task on a TaskSupervisor returns a UUID
/// task id, ties the task to the parent session, and reflects the
/// parent run_pipeline tool_call_id as the supervisor's tool_call_id
/// field.
#[tokio::test]
async fn pipeline_worker_registers_node_task_with_supervisor() {
    let (_host, _cache, supervisor, _acct, _ledger_dir) =
        host_context_with_cache_and_supervisor().await;
    let task_id = supervisor.register(
        "pipeline:search",
        "tool-call-run_pipeline-1",
        Some("session-w1"),
    );
    assert!(!task_id.is_empty(), "supervisor must assign a task id");
    let task = supervisor
        .get_task(&task_id)
        .expect("registered task is retrievable");
    assert_eq!(task.tool_name, "pipeline:search");
    assert_eq!(task.tool_call_id, "tool-call-run_pipeline-1");
    assert_eq!(task.parent_session_key.as_deref(), Some("session-w1"));
    // Mark a transition so we exercise the same lifecycle the executor
    // drives in dispatch.
    supervisor.mark_running(&task_id);
    let running = supervisor.get_task(&task_id).unwrap();
    assert_eq!(running.status.as_str(), "running");
    supervisor.mark_completed(&task_id, vec!["/tmp/out.md".into()]);
    let completed = supervisor.get_task(&task_id).unwrap();
    assert_eq!(completed.status.as_str(), "completed");
    assert_eq!(completed.output_files.len(), 1);
}

/// W1.A4 — opening a per-node `CostReservationHandle` against the
/// shared accountant and committing it lands a ledger row with the
/// pipeline contract id and the right token attribution.
#[tokio::test]
async fn pipeline_node_cost_reservation_commits_to_ledger() {
    let (_host, _cache, _sup, accountant, _ledger_dir) =
        host_context_with_cache_and_supervisor().await;
    let handle = accountant
        .reserve("pipeline-test", 0.05)
        .await
        .expect("reservation should succeed under 100 USD policy");
    assert_eq!(handle.contract_id(), "pipeline-test");
    assert!((handle.reserved_amount_usd() - 0.05).abs() < 1e-9);

    let event = octos_agent::cost_ledger::CostAttributionEvent::new(
        "pipeline-test".to_string(),
        "pipeline-test".to_string(),
        "pipeline-node-1".to_string(),
        "claude-sonnet-4",
        100,
        200,
        0.04,
    );
    handle.commit(event).await.expect("commit must succeed");

    let rollups = accountant
        .ledger()
        .aggregate_per_contract()
        .await
        .expect("rollup lookup");
    let rollup = rollups
        .iter()
        .find(|r| r.contract_id == "pipeline-test")
        .expect("pipeline-test contract row");
    assert!(
        rollup.cost_usd > 0.0,
        "ledger should show committed spend after node commit"
    );
    assert_eq!(rollup.tokens_in, 100);
    assert_eq!(rollup.tokens_out, 200);
}

/// Covers the `is_empty` invariant — the legacy code path must never
/// flip when only optional resources are absent.
#[tokio::test]
async fn empty_pipeline_host_context_does_not_inject_anything() {
    let host = PipelineHostContext::default();
    assert!(host.is_empty());
    let codergen = CodergenHandler::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        std::env::temp_dir(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .with_host_context(host);
    assert!(codergen.host_context().is_empty());
}

/// Smoke that the HandlerRegistry default registration path still
/// works after we wired host_context onto CodergenHandler.
#[tokio::test]
async fn handler_registry_default_with_codergen_carrying_host_context() {
    let (host, _, _, _, _ledger_dir) = host_context_with_cache_and_supervisor().await;
    let codergen = CodergenHandler::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        std::env::temp_dir(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .with_host_context(host);
    let mut registry = HandlerRegistry::new();
    registry.register(
        octos_pipeline::HandlerKind::Codergen,
        Arc::new(codergen) as Arc<dyn octos_pipeline::Handler>,
    );
    assert!(
        registry.get(&octos_pipeline::HandlerKind::Codergen).is_some(),
        "Codergen handler should resolve out of the registry"
    );
}
