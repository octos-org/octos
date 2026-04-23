//! Integration tests for the cost / provenance ledger (M7.4).
//!
//! These tests cover the full deliverable stack:
//!
//! - Redb-backed ledger survives open / close cycles.
//! - Every sub-agent dispatch that lands with `outcome == success`
//!   records an attribution row keyed off the supervising session and
//!   the contract id.
//! - Budget policies reject spawns before dispatch when the projected
//!   cost exceeds the ceiling; absent a policy, dispatches pass through
//!   unchanged (preserving the M7.1 dispatch contract).
//! - The typed `HarnessEventPayload::CostAttribution` variant round
//!   trips through the harness event pipeline.
//! - The operator summary extension aggregates per-contract spend from
//!   ledger rollups.

#![cfg(unix)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use octos_agent::cost_ledger::{
    BudgetProjection, CostAccountant, CostAttributionEvent, CostBudgetPolicy, CostLedger,
    PersistentCostLedger,
};
use octos_agent::harness_events::{HarnessCostAttributionEvent, HarnessEvent, HarnessEventPayload};
use octos_agent::tools::Tool;
use octos_agent::tools::mcp_agent::{
    DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackend,
};
use octos_agent::{SpawnTool, abi_schema};
use octos_core::InboundMessage;
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;

// ── Mock backend ───────────────────────────────────────────────────────────

/// In-process MCP backend double that returns a canned
/// [`DispatchResponse`]. Avoids the stdio / HTTP plumbing so ledger
/// tests focus on attribution logic rather than transport.
struct StaticBackend {
    response: DispatchResponse,
    label: &'static str,
}

#[async_trait]
impl McpAgentBackend for StaticBackend {
    fn backend_label(&self) -> &'static str {
        self.label
    }

    fn endpoint_label(&self) -> String {
        "fake://test".into()
    }

    async fn dispatch(&self, _request: DispatchRequest) -> DispatchResponse {
        self.response.clone()
    }
}

fn ready_response(text: &str) -> DispatchResponse {
    DispatchResponse {
        outcome: DispatchOutcome::Success,
        output: text.into(),
        files_to_send: Vec::new(),
        error: None,
    }
}

// ── Mock LLM provider ──────────────────────────────────────────────────────

struct NoopLlm;

#[async_trait]
impl LlmProvider for NoopLlm {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        Ok(ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    fn context_window(&self) -> u32 {
        32_000
    }

    fn model_id(&self) -> &str {
        "claude-haiku"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

async fn fresh_memory(dir: &tempfile::TempDir) -> Arc<EpisodeStore> {
    Arc::new(
        EpisodeStore::open(dir.path().join(".octos"))
            .await
            .expect("open memory"),
    )
}

fn inbound_channel() -> tokio::sync::mpsc::Sender<InboundMessage> {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    tx
}

fn dispatch_args(model: &str) -> serde_json::Value {
    serde_json::json!({
        "task": "Compute the answer to 7 * 6",
        "label": "test-subagent",
        "mode": "background",
        "backend": "agent_mcp",
        "allowed_tools": [],
        "model": model,
        "workflow": {
            "workflow_kind": "coding",
            "current_phase": "dispatch",
            "allowed_tools": [],
        },
    })
}

// ── Basic ledger invariants ────────────────────────────────────────────────

#[tokio::test]
async fn should_persist_ledger_across_process_restart() {
    let dir = tempfile::tempdir().unwrap();
    let attribution_id;

    {
        let ledger = PersistentCostLedger::open(dir.path()).await.unwrap();
        let event = CostAttributionEvent::new(
            "session-abc",
            "contract-one",
            "task-x",
            "claude-haiku",
            1_000,
            500,
            0.01,
        );
        attribution_id = event.attribution_id.clone();
        ledger.record(event).await.unwrap();
        // Drop to flush the database.
    }

    // Reopen and verify the attribution is still readable.
    let ledger = PersistentCostLedger::open(dir.path()).await.unwrap();
    let rows = ledger.list_for_contract("contract-one").await.unwrap();
    assert_eq!(rows.len(), 1, "reopened ledger lost rows");
    assert_eq!(rows[0].attribution_id, attribution_id);
    assert_eq!(rows[0].supervisor_session, "session-abc");
    assert_eq!(rows[0].tokens_in, 1_000);
    assert_eq!(rows[0].tokens_out, 500);
}

#[tokio::test]
async fn should_aggregate_cost_per_contract_in_operator_summary() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = PersistentCostLedger::open(dir.path()).await.unwrap();

    let mut event_a1 = CostAttributionEvent::new(
        "session-1",
        "contract-A",
        "task-1",
        "claude-haiku",
        1_000,
        500,
        0.02,
    );
    event_a1.cost_usd = 0.02;
    let mut event_a2 = CostAttributionEvent::new(
        "session-1",
        "contract-A",
        "task-2",
        "claude-haiku",
        500,
        250,
        0.01,
    );
    event_a2.cost_usd = 0.01;
    let mut event_b1 = CostAttributionEvent::new(
        "session-2",
        "contract-B",
        "task-3",
        "claude-sonnet-4",
        10_000,
        2_500,
        0.50,
    );
    event_b1.cost_usd = 0.50;

    ledger.record(event_a1).await.unwrap();
    ledger.record(event_a2).await.unwrap();
    ledger.record(event_b1).await.unwrap();

    let rollups = ledger.aggregate_per_contract().await.unwrap();
    assert_eq!(rollups.len(), 2);
    // Highest spend first — contract-B at $0.50.
    assert_eq!(rollups[0].contract_id, "contract-B");
    assert_eq!(rollups[0].dispatch_count, 1);
    assert!((rollups[0].cost_usd - 0.50).abs() < 1e-9);

    assert_eq!(rollups[1].contract_id, "contract-A");
    assert_eq!(rollups[1].dispatch_count, 2);
    assert!((rollups[1].cost_usd - 0.03).abs() < 1e-9);
    assert_eq!(rollups[1].tokens_in, 1_500);
    assert_eq!(rollups[1].tokens_out, 750);
}

// ── Typed harness event round trip ─────────────────────────────────────────

#[tokio::test]
async fn should_emit_typed_cost_attribution_event() {
    let data = HarnessCostAttributionEvent {
        schema_version: abi_schema::COST_ATTRIBUTION_SCHEMA_VERSION,
        session_id: "session-e2e".into(),
        task_id: "task-e2e".into(),
        workflow: Some("coding".into()),
        phase: Some("dispatch".into()),
        attribution_id: "cost-e2e-1".into(),
        contract_id: "contract-A".into(),
        model: "claude-haiku".into(),
        tokens_in: 2_500,
        tokens_out: 1_250,
        cost_usd: 0.04,
        outcome: "success".into(),
        extra: HashMap::new(),
    };
    let event = HarnessEvent::cost_attribution(data.clone());
    event
        .validate()
        .expect("cost attribution event must validate");

    // Round trip through JSON to make sure the tag is surfaced in the
    // on-wire format downstream harness consumers read from the sink.
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"kind\":\"cost_attribution\""));

    let parsed: HarnessEvent = serde_json::from_str(&json).unwrap();
    match parsed.payload {
        HarnessEventPayload::CostAttribution { data: parsed } => {
            assert_eq!(parsed.attribution_id, data.attribution_id);
            assert_eq!(parsed.contract_id, data.contract_id);
            assert_eq!(parsed.model, data.model);
            assert_eq!(parsed.tokens_in, data.tokens_in);
            assert_eq!(parsed.tokens_out, data.tokens_out);
            assert!((parsed.cost_usd - data.cost_usd).abs() < 1e-9);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[tokio::test]
async fn should_reject_cost_attribution_event_with_negative_cost() {
    let event = HarnessEvent::cost_attribution(HarnessCostAttributionEvent {
        schema_version: abi_schema::COST_ATTRIBUTION_SCHEMA_VERSION,
        session_id: "session".into(),
        task_id: "task".into(),
        workflow: None,
        phase: None,
        attribution_id: "cost-neg".into(),
        contract_id: "contract".into(),
        model: "model".into(),
        tokens_in: 0,
        tokens_out: 0,
        cost_usd: -1.0,
        outcome: "success".into(),
        extra: HashMap::new(),
    });
    let err = event.validate().expect_err("must reject negative cost");
    let rendered = err.to_string();
    assert!(
        rendered.contains("non-negative"),
        "unexpected error: {rendered}"
    );
}

// ── End-to-end dispatch path ───────────────────────────────────────────────

#[tokio::test]
async fn should_record_cost_attribution_on_sub_agent_dispatch_completion() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(PersistentCostLedger::open(dir.path()).await.unwrap());
    let accountant = Arc::new(CostAccountant::new(ledger.clone(), None));

    let backend: Arc<dyn McpAgentBackend> = Arc::new(StaticBackend {
        response: ready_response("sub-agent produced the artifact"),
        label: "local",
    });

    let llm: Arc<dyn LlmProvider> = Arc::new(NoopLlm);
    let memory = fresh_memory(&dir).await;
    let tx = inbound_channel();

    let spawn = SpawnTool::new(llm, memory, dir.path().to_path_buf(), tx)
        .with_mcp_agent_backend(backend, None)
        .with_cost_accountant(accountant.clone());

    let result = spawn.execute(&dispatch_args("claude-haiku")).await.unwrap();
    assert!(result.success, "dispatch should succeed: {}", result.output);

    // Give the redb spawn_blocking a beat to commit.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let rows = accountant
        .ledger()
        .list_for_contract("coding")
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one attribution row, got {rows:?}"
    );
    assert_eq!(rows[0].model, "claude-haiku");
    assert_eq!(rows[0].contract_id, "coding");
    assert!(rows[0].tokens_in > 0, "tokens_in must be estimated");
    assert_eq!(rows[0].outcome.as_deref(), Some("success"));
    assert_eq!(rows[0].backend.as_deref(), Some("local"));
}

#[tokio::test]
async fn should_allow_dispatch_when_budget_policy_absent() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(PersistentCostLedger::open(dir.path()).await.unwrap());
    let accountant = Arc::new(CostAccountant::new(ledger.clone(), None));

    let backend: Arc<dyn McpAgentBackend> = Arc::new(StaticBackend {
        response: ready_response("ok"),
        label: "local",
    });
    let llm: Arc<dyn LlmProvider> = Arc::new(NoopLlm);
    let memory = fresh_memory(&dir).await;
    let tx = inbound_channel();

    let spawn = SpawnTool::new(llm, memory, dir.path().to_path_buf(), tx)
        .with_mcp_agent_backend(backend, None)
        .with_cost_accountant(accountant.clone());

    let result = spawn.execute(&dispatch_args("claude-haiku")).await.unwrap();
    assert!(
        result.success,
        "absent policy must not reject dispatches: {}",
        result.output
    );
}

#[tokio::test]
async fn should_fail_dispatch_when_budget_policy_exceeded() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(PersistentCostLedger::open(dir.path()).await.unwrap());
    // A ceiling small enough that any non-zero projection trips it.
    let policy = CostBudgetPolicy::default().with_per_dispatch_usd(1e-12);
    let accountant = Arc::new(CostAccountant::new(ledger.clone(), Some(policy)));

    let backend: Arc<dyn McpAgentBackend> = Arc::new(StaticBackend {
        response: ready_response("should not be called"),
        label: "local",
    });
    let llm: Arc<dyn LlmProvider> = Arc::new(NoopLlm);
    let memory = fresh_memory(&dir).await;
    let tx = inbound_channel();

    let spawn = SpawnTool::new(llm, memory, dir.path().to_path_buf(), tx)
        .with_mcp_agent_backend(backend, None)
        .with_cost_accountant(accountant.clone());

    let result = spawn.execute(&dispatch_args("claude-haiku")).await.unwrap();
    assert!(
        !result.success,
        "budget policy should have rejected the dispatch: {}",
        result.output
    );
    assert!(
        result.output.to_lowercase().contains("budget"),
        "reason should mention budget: {}",
        result.output
    );

    // Ledger should remain empty because the dispatch never ran.
    let rows = accountant
        .ledger()
        .list_for_contract("coding")
        .await
        .unwrap();
    assert!(rows.is_empty(), "unexpected ledger rows: {rows:?}");
}

// ── Budget policy unit coverage ────────────────────────────────────────────

#[tokio::test]
async fn should_project_budget_from_historical_spend() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(PersistentCostLedger::open(dir.path()).await.unwrap());
    let policy = CostBudgetPolicy::default()
        .with_per_contract_usd(0.05)
        .with_global_usd(0.20);
    let accountant = CostAccountant::new(ledger.clone(), Some(policy));

    let existing = CostAttributionEvent::new(
        "session-1",
        "contract-A",
        "task-seed",
        "claude-haiku",
        100,
        50,
        0.04,
    );
    ledger.record(existing).await.unwrap();

    // projected_usd + 0.04 = 0.05 which equals the per-contract cap, not over.
    match accountant
        .project_dispatch("contract-A", 0.01)
        .await
        .unwrap()
    {
        BudgetProjection::Allowed { .. } => {}
        other => panic!("expected allowed at exact cap, got {other:?}"),
    }

    // projected_usd + 0.04 = 0.06 which exceeds the per-contract cap.
    match accountant
        .project_dispatch("contract-A", 0.03)
        .await
        .unwrap()
    {
        BudgetProjection::Rejected { reason, .. } => {
            let rendered = format!("{reason}");
            assert!(
                rendered.contains("per-contract budget"),
                "wrong reason: {rendered}"
            );
        }
        other => panic!("expected rejection, got {other:?}"),
    }
}
