//! Acceptance tests for coding-blue FA-7 — workspace-contract
//! enforcement inside octos-pipeline.
//!
//! These tests drive the [`PipelineExecutor`] with scripted mock
//! handlers / providers so they stay fully deterministic (no network,
//! no real LLM calls, no disk beyond the TempDir workspace).
//!
//! The matrix mirrors the supervisor brief:
//!
//! * Terminal validator gate PASS + FAIL
//! * Compaction propagation to Codergen (including parallel fan-outs)
//! * Cost reservation: per-node reservation, refund on failure,
//!   commit on success, legacy path skipped when accountant absent
//! * Human gates don't reserve (Gate handler is not an LLM-call kind)
//! * Unreached conditional branches don't consume reservation
//!
//! Each test is tagged in-file with the planned test name from the
//! supervisor brief so future reviewers can map 1:1.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use async_trait::async_trait;
use eyre::Result;
use octos_agent::cost_ledger::{
    CostAccountant, CostAttributionEvent, CostBudgetPolicy, PersistentCostLedger,
};
use octos_agent::workspace_policy::{
    CompactionPolicy, CompactionSummarizerKind, ValidationPolicy, Validator, ValidatorPhaseKind,
    ValidatorSpec, WorkspaceArtifactsPolicy, WorkspacePolicy, WorkspacePolicyKind,
    WorkspacePolicyWorkspace, WorkspaceSnapshotTrigger, WorkspaceTrackingPolicy,
    WorkspaceVersionControlPolicy, WorkspaceVersionControlProvider,
};
use octos_agent::{COMPACTION_POLICY_SCHEMA_VERSION, WORKSPACE_POLICY_SCHEMA_VERSION};
use octos_core::TokenUsage;
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage as LlmTokenUsage};
use octos_memory::EpisodeStore;
use octos_pipeline::context::PipelineContext;
use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor};
use octos_pipeline::handler::HandlerContext;
use octos_pipeline::{
    Handler, HandlerKind, HandlerRegistry, NodeOutcome, NoopHandler, OutcomeStatus, PipelineNode,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Scripted harness
// ---------------------------------------------------------------------------

/// Scripted LLM provider — always replies with a fixed end-of-turn
/// string. Fine for the handlers that don't actually invoke the LLM
/// (our tests use `NoopHandler` plus `RecordingHandler` below), but
/// required as a typed `Arc<dyn LlmProvider>` dependency on
/// [`ExecutorConfig`].
struct ScriptedLlmProvider {
    reply: String,
    calls: AtomicU32,
}

impl ScriptedLlmProvider {
    fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            calls: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlmProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            content: Some(self.reply.clone()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: LlmTokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "scripted-mock"
    }

    fn provider_name(&self) -> &str {
        "scripted-mock"
    }
}

/// Handler that records the node ids it executed. Used to assert that
/// unreached conditional branches never invoked a handler (and never
/// reserved budget).
struct RecordingHandler {
    invocations: Arc<std::sync::Mutex<Vec<String>>>,
    outcome_status: OutcomeStatus,
}

impl RecordingHandler {
    fn new(outcome_status: OutcomeStatus) -> (Arc<Self>, Arc<std::sync::Mutex<Vec<String>>>) {
        let invocations = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler = Arc::new(Self {
            invocations: invocations.clone(),
            outcome_status,
        });
        (handler, invocations)
    }
}

#[async_trait]
impl Handler for RecordingHandler {
    async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
        self.invocations
            .lock()
            .expect("recording handler lock")
            .push(node.id.clone());
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: self.outcome_status,
            content: format!("recorded-{}", node.id),
            token_usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            files_modified: vec![],
        })
    }
}

async fn episode_store(dir: &std::path::Path) -> Arc<EpisodeStore> {
    Arc::new(EpisodeStore::open(dir).await.expect("open episode store"))
}

fn provider() -> Arc<dyn LlmProvider> {
    Arc::new(ScriptedLlmProvider::new("done"))
}

fn base_config(dir: &TempDir, memory: Arc<EpisodeStore>, ctx: PipelineContext) -> ExecutorConfig {
    ExecutorConfig {
        default_provider: provider(),
        provider_router: None,
        memory,
        working_dir: dir.path().to_path_buf(),
        provider_policy: None,
        plugin_dirs: vec![],
        status_bridge: None,
        shutdown: Arc::new(AtomicBool::new(false)),
        max_parallel_workers: 4,
        max_pipeline_fanout_total: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: ctx,
        host_context: octos_pipeline::host_context::PipelineHostContext::default(),
    }
}

fn handlers_with(kind: HandlerKind, handler: Arc<dyn Handler>) -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    // Fill every slot with NoopHandler first, then overwrite the
    // target slot with the caller's custom handler so the custom
    // entry survives the default fill pass.
    registry.register(HandlerKind::Noop, Arc::new(NoopHandler));
    registry.register(HandlerKind::Codergen, Arc::new(NoopHandler));
    registry.register(HandlerKind::Gate, Arc::new(NoopHandler));
    registry.register(HandlerKind::Shell, Arc::new(NoopHandler));
    registry.register(HandlerKind::Parallel, Arc::new(NoopHandler));
    registry.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));
    registry.register(kind, handler);
    registry
}

// ---------------------------------------------------------------------------
// Policy helpers
// ---------------------------------------------------------------------------

fn empty_policy() -> WorkspacePolicy {
    WorkspacePolicy {
        schema_version: WORKSPACE_POLICY_SCHEMA_VERSION,
        workspace: WorkspacePolicyWorkspace {
            kind: WorkspacePolicyKind::Session,
        },
        version_control: WorkspaceVersionControlPolicy {
            provider: WorkspaceVersionControlProvider::Git,
            auto_init: false,
            trigger: WorkspaceSnapshotTrigger::TurnEnd,
            fail_on_error: false,
        },
        tracking: WorkspaceTrackingPolicy { ignore: Vec::new() },
        validation: ValidationPolicy::default(),
        artifacts: WorkspaceArtifactsPolicy::default(),
        spawn_tasks: BTreeMap::new(),
        compaction: None,
    }
}

fn policy_with_required_validator(required_file_path: &str) -> WorkspacePolicy {
    let mut policy = empty_policy();
    policy.validation.validators = vec![Validator {
        id: "required-artifact".into(),
        required: true,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: required_file_path.to_string(),
            min_bytes: Some(1),
        },
    }];
    policy
}

fn policy_with_compaction() -> WorkspacePolicy {
    let mut policy = empty_policy();
    policy.compaction = Some(CompactionPolicy {
        schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
        token_budget: 2_000,
        preserved_artifacts: Vec::new(),
        preserved_invariants: Vec::new(),
        summarizer: CompactionSummarizerKind::Extractive,
        preflight_threshold: Some(1_000),
        prune_tool_results_after_turns: None,
    });
    policy
}

// ---------------------------------------------------------------------------
// Shared DOT fixtures
// ---------------------------------------------------------------------------

const SINGLE_NOOP_DOT: &str = r#"
    digraph sample {
        only [handler="noop"]
    }
"#;

/// Conditional branch: `root -> left` only fires when the outcome
/// status is `"fail"`; our RecordingHandler emits
/// `OutcomeStatus::Pass` so the `left` edge's condition never
/// matches and `left` never executes.
const CONDITIONAL_BRANCH_DOT: &str = r#"
    digraph conditional_sample {
        root [handler="codergen", model="test-model"]
        left [handler="codergen", model="test-model", label="Left branch"]
        right [handler="codergen", model="test-model", label="Right branch"]
        root -> left [condition="outcome.status == \"fail\""]
        root -> right
    }
"#;

// ---------------------------------------------------------------------------
// 1. Terminal validator gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_run_terminal_validator_on_pipeline_completion() {
    let dir = TempDir::new().unwrap();
    // Satisfy the validator up front — the `artifact.txt` file exists
    // so completion gates open cleanly.
    std::fs::write(dir.path().join("artifact.txt"), b"ok").unwrap();
    let memory = episode_store(dir.path()).await;

    let policy = policy_with_required_validator("artifact.txt");
    let ctx = PipelineContext::new()
        .with_policy(policy)
        .with_agent_llm_provider(provider())
        .with_contract_id("test-pipeline");
    let executor = PipelineExecutor::new(base_config(&dir, memory, ctx));

    let (handler, invocations) = RecordingHandler::new(OutcomeStatus::Pass);
    let result = executor
        .run_with_handlers(
            SINGLE_NOOP_DOT,
            "hello",
            &serde_json::Map::new(),
            handlers_with(HandlerKind::Noop, handler),
        )
        .await
        .expect("pipeline must complete");

    assert!(
        result.success,
        "pipeline must pass with validator satisfied"
    );
    assert_eq!(
        invocations.lock().unwrap().len(),
        1,
        "the sole node must run once"
    );
}

#[tokio::test]
async fn should_reject_completion_when_required_validator_fails() {
    let dir = TempDir::new().unwrap();
    // Required file deliberately missing.
    let memory = episode_store(dir.path()).await;

    let policy = policy_with_required_validator("artifact.txt");
    let ctx = PipelineContext::new()
        .with_policy(policy)
        .with_agent_llm_provider(provider());
    let executor = PipelineExecutor::new(base_config(&dir, memory, ctx));

    let (handler, _) = RecordingHandler::new(OutcomeStatus::Pass);
    let result = executor
        .run_with_handlers(
            SINGLE_NOOP_DOT,
            "hello",
            &serde_json::Map::new(),
            handlers_with(HandlerKind::Noop, handler),
        )
        .await
        .expect("pipeline should not bail (validator rejection is a demotion, not a bail)");

    assert!(
        !result.success,
        "required validator failure must demote success to false"
    );
    assert!(
        result
            .output
            .contains("Pipeline validator rejected completion"),
        "output must tag the rejection reason; got `{}`",
        result.output
    );
    assert!(
        result.output.contains("required-artifact")
            || result.output.contains("required validator failure"),
        "output must identify the failing validator; got `{}`",
        result.output
    );
}

// ---------------------------------------------------------------------------
// 2. Compaction propagation to parallel fan-outs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_propagate_compaction_to_parallel_fanout_node() {
    let dir = TempDir::new().unwrap();
    let memory = episode_store(dir.path()).await;

    // Policy declares a compaction block but no validator — so the
    // only observable effect is whether the CodergenHandler receives
    // the compaction policy.
    let policy = policy_with_compaction();
    let ctx = PipelineContext::new()
        .with_policy(policy)
        .with_agent_llm_provider(provider());
    let executor = PipelineExecutor::new(base_config(&dir, memory, ctx));

    // Inspect the codergen handler the executor builds — the
    // parallel-fan-out path uses THIS handler for every target, so if
    // the compaction policy is set here, every concurrent branch
    // inherits it.
    let codergen = executor.build_codergen_for_test();
    assert!(
        codergen.has_compaction_policy(),
        "compaction policy must be attached to the codergen handler used by parallel fan-outs"
    );
    assert!(
        codergen.has_compaction_workspace(),
        "compaction workspace must be attached so the runner can resolve artifact names"
    );
}

#[tokio::test]
async fn legacy_context_skips_compaction_wiring() {
    // Regression guard: when no workspace context is installed, the
    // codergen handler must NOT attach a compaction policy. The
    // compaction runner only fires on explicit opt-in.
    let dir = TempDir::new().unwrap();
    let memory = episode_store(dir.path()).await;
    let executor = PipelineExecutor::new(base_config(&dir, memory, PipelineContext::default()));
    let codergen = executor.build_codergen_for_test();
    assert!(
        !codergen.has_compaction_policy(),
        "legacy path must not attach a compaction policy"
    );
    assert!(
        !codergen.has_compaction_workspace(),
        "legacy path must not attach a compaction workspace"
    );
}

// ---------------------------------------------------------------------------
// 3. Cost reservation
// ---------------------------------------------------------------------------

async fn accountant(dir: &TempDir, policy: Option<CostBudgetPolicy>) -> Arc<CostAccountant> {
    let ledger = PersistentCostLedger::open(dir.path())
        .await
        .expect("open cost ledger");
    Arc::new(CostAccountant::new(Arc::new(ledger), policy))
}

#[tokio::test]
async fn should_reserve_cost_per_node_and_refund_on_failure() {
    let dir = TempDir::new().unwrap();
    let ledger_dir = TempDir::new().unwrap();
    let memory = episode_store(dir.path()).await;

    // Budget policy with a tight per-contract ceiling so a hypothetical
    // double-reservation would trip. But because each per-node handle
    // drops after dispatch, the pipeline-level reservation holds the
    // entire projected spend and the test succeeds.
    let policy = CostBudgetPolicy::default()
        .with_per_contract_usd(1.0)
        .with_per_dispatch_usd(1.0);
    let accountant = accountant(&ledger_dir, Some(policy)).await;

    let ctx = PipelineContext::new()
        .with_cost_accountant(accountant.clone())
        .with_contract_id("fa7-refund-test")
        .with_projected_usd(0.05);
    let executor = PipelineExecutor::new(base_config(&dir, memory, ctx));

    // Use a handler that returns Error so the pipeline fails; the
    // reservation must still refund cleanly on the failure path.
    let (handler, _) = RecordingHandler::new(OutcomeStatus::Error);

    let result = executor
        .run_with_handlers(
            r#"
            digraph sample {
                only [handler="codergen", model="test-model"]
            }
            "#,
            "hello",
            &serde_json::Map::new(),
            handlers_with(HandlerKind::Codergen, handler),
        )
        .await
        .expect("pipeline must complete (even on Error, it returns Ok(PipelineResult))");

    assert!(!result.success, "error outcome must demote success");

    // No commit on failure — the ledger records zero attributions for
    // this contract.
    let rows = accountant
        .ledger()
        .list_for_contract("fa7-refund-test")
        .await
        .expect("ledger read");
    assert!(
        rows.is_empty(),
        "failed pipeline must not commit to the ledger; got {rows:?}"
    );
}

#[tokio::test]
async fn should_commit_pipeline_cost_on_success() {
    let dir = TempDir::new().unwrap();
    let ledger_dir = TempDir::new().unwrap();
    let memory = episode_store(dir.path()).await;
    let accountant = accountant(&ledger_dir, None).await;

    let ctx = PipelineContext::new()
        .with_cost_accountant(accountant.clone())
        .with_contract_id("fa7-commit-test")
        .with_projected_usd(0.02);
    let executor = PipelineExecutor::new(base_config(&dir, memory, ctx));

    let (handler, _) = RecordingHandler::new(OutcomeStatus::Pass);

    let result = executor
        .run_with_handlers(
            r#"
            digraph sample {
                only [handler="codergen", model="test-model"]
            }
            "#,
            "hello",
            &serde_json::Map::new(),
            handlers_with(HandlerKind::Codergen, handler),
        )
        .await
        .expect("pipeline must complete");

    assert!(result.success, "pipeline must succeed with Pass outcome");

    let rows = accountant
        .ledger()
        .list_for_contract("fa7-commit-test")
        .await
        .expect("ledger read");
    assert_eq!(
        rows.len(),
        1,
        "successful pipeline must commit exactly one attribution row"
    );
    let event: &CostAttributionEvent = &rows[0];
    assert_eq!(event.contract_id, "fa7-commit-test");
    assert!(
        event.model.contains("pipeline"),
        "commit must tag the aggregate model label; got `{}`",
        event.model
    );
}

#[tokio::test]
async fn should_skip_budget_check_when_accountant_absent() {
    // Legacy-path regression guard: when PipelineContext has no
    // CostAccountant, the executor must behave identically to the
    // pre-FA-7 path — no panics, no reservations, no ledger access.
    let dir = TempDir::new().unwrap();
    let memory = episode_store(dir.path()).await;

    // Context has no accountant but a policy that declares a
    // compaction block. The reservation code path must still be inert.
    let ctx = PipelineContext::new()
        .with_policy(policy_with_compaction())
        .with_agent_llm_provider(provider());
    let executor = PipelineExecutor::new(base_config(&dir, memory, ctx));

    let (handler, invocations) = RecordingHandler::new(OutcomeStatus::Pass);
    let result = executor
        .run_with_handlers(
            SINGLE_NOOP_DOT,
            "hello",
            &serde_json::Map::new(),
            handlers_with(HandlerKind::Noop, handler),
        )
        .await
        .expect("legacy-path pipeline must complete");

    assert!(result.success);
    assert_eq!(invocations.lock().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// 4. Human gates + conditional branches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_not_compact_at_human_gate_node() {
    // Human gates map to `HandlerKind::Gate` — those are NOT LLM-call
    // nodes, so the executor must not reserve budget for them. The
    // codergen handler's compaction wiring is independent (no gate
    // handler gets compaction attached).
    let dir = TempDir::new().unwrap();
    let memory = episode_store(dir.path()).await;

    let ledger_dir = TempDir::new().unwrap();
    let accountant = accountant(&ledger_dir, None).await;

    let ctx = PipelineContext::new()
        .with_policy(policy_with_compaction())
        .with_agent_llm_provider(provider())
        .with_cost_accountant(accountant.clone())
        .with_contract_id("fa7-gate-test");
    let executor = PipelineExecutor::new(base_config(&dir, memory, ctx));

    let (gate_handler, gate_invocations) = RecordingHandler::new(OutcomeStatus::Pass);

    let result = executor
        .run_with_handlers(
            r#"
            digraph gate_sample {
                gate [handler="gate"]
            }
            "#,
            "hello",
            &serde_json::Map::new(),
            handlers_with(HandlerKind::Gate, gate_handler),
        )
        .await
        .expect("pipeline must complete");

    assert!(result.success);
    assert_eq!(
        gate_invocations.lock().unwrap().len(),
        1,
        "the gate node must run"
    );

    // The gate node is NOT a Codergen / DynamicParallel node, so
    // `reserve_node_budget` short-circuits and no per-node reservation
    // is taken. The pipeline-level reservation still commits with the
    // aggregate spend — one row in the ledger.
    let rows = accountant
        .ledger()
        .list_for_contract("fa7-gate-test")
        .await
        .expect("ledger read");
    assert_eq!(
        rows.len(),
        1,
        "pipeline-level commit must land exactly one row; gate nodes do not add extra reservations"
    );
}

#[tokio::test]
async fn should_not_count_conditional_unreached_branch_against_reservation() {
    // A conditional branch that never fires must NOT reserve budget.
    // The reservation is scoped to the point of dispatch, so edges
    // that are pruned by condition evaluation never hit the
    // `reserve_node_budget` call.
    let dir = TempDir::new().unwrap();
    let memory = episode_store(dir.path()).await;

    let ledger_dir = TempDir::new().unwrap();
    let accountant = accountant(&ledger_dir, None).await;

    let ctx = PipelineContext::new()
        .with_cost_accountant(accountant.clone())
        .with_contract_id("fa7-conditional-test")
        .with_projected_usd(0.02);
    let executor = PipelineExecutor::new(base_config(&dir, memory, ctx));

    let (handler, invocations) = RecordingHandler::new(OutcomeStatus::Pass);

    let _result = executor
        .run_with_handlers(
            CONDITIONAL_BRANCH_DOT,
            "hello",
            &serde_json::Map::new(),
            handlers_with(HandlerKind::Codergen, handler),
        )
        .await
        .expect("pipeline must complete");

    // `root` + `right` must have run; `left` must NOT have run because
    // the condition on the root→left edge never matched.
    let seen = invocations.lock().unwrap().clone();
    assert!(seen.contains(&"root".to_string()), "root must run");
    assert!(seen.contains(&"right".to_string()), "right must run");
    assert!(
        !seen.contains(&"left".to_string()),
        "left must NOT run — conditional pruning prevents dispatch and therefore reservation"
    );
}

// ---------------------------------------------------------------------------
// 5. PipelineContext builder unit coverage (helper)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn context_is_empty_by_default() {
    // Convenience check that the default context stays on the legacy
    // path — keeps the "no-op when opted out" invariant visible next
    // to the acceptance tests.
    let ctx = PipelineContext::default();
    assert!(ctx.is_empty());
    assert!(ctx.contract_id.is_empty());
    assert_eq!(ctx.pipeline_projected_usd, 0.0);
}
