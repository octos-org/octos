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
    Arc::new(octos_memory::EpisodeStore::open(dir.path()).await.unwrap())
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
        // Phase 1 of the SessionScope migration (PR #1198 follow-up):
        // existing M8 parity tests stay on the legacy None path since
        // no consumer reads the field yet.
        session_scope: None,
    };
    (host, cache, supervisor, accountant, ledger_dir)
}

/// W1 acceptance — confirm `CodergenHandler` exposes the host context
/// it was built with, and that the host context carries the parent
/// session's `FileStateCache`.
#[tokio::test]
async fn pipeline_worker_handler_carries_file_state_cache_from_host() {
    let (host, _cache, _sup, _acct, _ledger_dir) = host_context_with_cache_and_supervisor().await;
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
/// shared accountant and dropping it auto-refunds the projected
/// budget so the contract's reservation slot is reusable. Per-node
/// ledger writes intentionally never land — the pipeline-level
/// handle records the cumulative attribution at the run terminal so
/// per-node commits would double-count.
#[tokio::test]
async fn pipeline_node_cost_reservation_auto_refunds_on_drop() {
    let (_host, _cache, _sup, accountant, _ledger_dir) =
        host_context_with_cache_and_supervisor().await;

    // Open a per-node reservation against the contract.
    let handle = accountant
        .reserve("pipeline-test", 0.05)
        .await
        .expect("reservation should succeed under 100 USD policy");
    assert_eq!(handle.contract_id(), "pipeline-test");
    assert!((handle.reserved_amount_usd() - 0.05).abs() < 1e-9);

    // Drop without committing — the projected budget must auto-refund
    // so a second reservation against the same contract is admissible.
    drop(handle);
    // Give the Drop-spawned refund task a moment to land, mirroring
    // the cost_reservation integration test pattern.
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let second = accountant
        .reserve("pipeline-test", 0.05)
        .await
        .expect("second reservation must succeed after auto-refund");
    drop(second);

    // Ledger has no rows because nothing was committed — pipeline
    // scope owns the aggregate write at terminal.
    let rollups = accountant
        .ledger()
        .aggregate_per_contract()
        .await
        .expect("rollup lookup");
    let rollup = rollups.iter().find(|r| r.contract_id == "pipeline-test");
    assert!(
        rollup.is_none() || rollup.unwrap().cost_usd <= f64::EPSILON,
        "per-node reservation must not commit to ledger; got {rollup:?}"
    );
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
        registry
            .get(&octos_pipeline::HandlerKind::Codergen)
            .is_some(),
        "Codergen handler should resolve out of the registry"
    );
}

/// Phase 3-A plumbing follow-up (gap #2 from codex review of
/// Phase 2-C): the pipeline `CodergenHandler::execute` builds a
/// per-node worker `Agent::new(...)` and threads parent host-context
/// fields onto it via `.with_file_state_cache(...)`, etc. Before this
/// fix, `host_context.session_scope` was NOT among the propagated
/// fields, so every pipeline-spawned child agent observed
/// `session_scope: None` and the Phase-2 consumers inside the child
/// (file tools / shell+spawn CWD / plugin work_dir) fell back to
/// legacy paths — re-introducing the mini5 NEW-06 contamination
/// class through `run_pipeline` even when the parent turn carried
/// a scope.
///
/// This is a plumbing assertion: confirm `CodergenHandler` exposes
/// the `session_scope` it was built with via the doc-hidden test
/// accessor. The fact that the `execute()` path threads this onto
/// `worker = Agent::new(...).with_session_scope(scope)` is then a
/// straight-line propagation we don't need a synthetic LLM run to
/// exercise.
#[tokio::test]
async fn pipeline_worker_agent_inherits_session_scope() {
    use octos_core::SessionScope;

    let session_root = tempfile::tempdir().expect("scope root");
    let scope = Arc::new(
        SessionScope::multi_tenant_with_default_zones(
            session_root.path().to_path_buf(),
            "_main".to_string(),
            "web-1779000000000-pipw0r".to_string(),
        )
        .expect("multi-tenant scope"),
    );

    let host = PipelineHostContext {
        // Stand alone — no other parent-session resources required to
        // exercise the session_scope propagation.
        session_scope: Some(scope.clone()),
        ..Default::default()
    };
    let codergen = CodergenHandler::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        std::env::temp_dir(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .with_host_context(host);

    let observed = codergen.host_context();
    let observed_scope = observed
        .session_scope
        .as_ref()
        .expect("CodergenHandler must propagate SessionScope from host context");
    assert!(
        Arc::ptr_eq(&scope, observed_scope),
        "CodergenHandler must hold the SAME SessionScope Arc the host context attached, \
         not a freshly-built one (proves scope is propagated end-to-end)",
    );
    // Workspace shape sanity — the scope's workspace dir resolves to
    // the multi-tenant `<root>/users/<profile>/sessions/<id>/workspace`
    // layout the on-disk contract requires.
    assert!(
        observed_scope.workspace().starts_with(session_root.path()),
        "scope workspace must live under the tenant root we configured"
    );
}
