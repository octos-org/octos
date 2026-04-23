//! Review A F-004 swarm-side acceptance tests.
//!
//! Locks in two additions to the swarm primitive:
//!
//! 1. Per-subtask completion validators. The pre-fix swarm only ran the
//!    aggregate validator after every subtask reached terminal state.
//!    A subtask that returned `Success` with a missing required artifact
//!    would float all the way to the aggregate gate, at which point a
//!    failure is ambiguous between "this subtask" and "an upstream one".
//!    The fix demotes a `Completed` subtask to `TerminalFailed` when any
//!    declared completion-phase validator rejects it, and records the
//!    failure reason on [`SubtaskOutcome::error`].
//! 2. Per-subtask cost reservation. The pre-fix swarm had no budget
//!    enforcement — concurrent subtasks could blow past the caller's
//!    per-contract cap. A `SwarmCostBudget` wired through `with_cost_budget`
//!    now funnels every subtask through
//!    [`CostAccountant::reserve`](octos_agent::cost_ledger::CostAccountant::reserve)
//!    so concurrent dispatches see each other's outstanding projections.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use octos_agent::cost_ledger::{
    CostAccountant, CostBudgetPolicy, CostLedger, PersistentCostLedger,
};
use octos_agent::tools::mcp_agent::{
    DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackend,
};
use octos_agent::validators::{ValidatorInvocation, ValidatorPhase, ValidatorRunner};
use octos_agent::workspace_policy::{Validator, ValidatorPhaseKind, ValidatorSpec};
use octos_swarm::{
    AggregateValidator, ContractSpec, Swarm, SwarmBudget, SwarmContext, SwarmCostBudget,
    SwarmOutcomeKind, SwarmTopology,
};

#[derive(Default)]
struct OkBackend;

#[async_trait]
impl McpAgentBackend for OkBackend {
    fn backend_label(&self) -> &'static str {
        "local"
    }

    fn endpoint_label(&self) -> String {
        "ok".to_string()
    }

    async fn dispatch(&self, request: DispatchRequest) -> DispatchResponse {
        let contract_id = request
            .task
            .get("contract_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        DispatchResponse {
            outcome: DispatchOutcome::Success,
            output: format!("ok:{contract_id}"),
            files_to_send: Vec::new(),
            error: None,
        }
    }
}

fn contract(id: &str) -> ContractSpec {
    ContractSpec {
        contract_id: id.into(),
        tool_name: "run".into(),
        task: serde_json::json!({ "contract_id": id }),
        label: Some(format!("c-{id}")),
    }
}

fn context() -> SwarmContext {
    SwarmContext {
        session_id: "api:subtask-test".into(),
        task_id: "task-1".into(),
        workflow: Some("subtask_test".into()),
        phase: Some("dispatch".into()),
    }
}

fn required_file_validator(id: &str, path: &str) -> Validator {
    Validator {
        id: id.into(),
        required: true,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: path.into(),
            min_bytes: Some(1),
        },
    }
}

fn aggregate_validator(
    workspace_dir: &std::path::Path,
    validators: Vec<Validator>,
) -> AggregateValidator {
    let tools = Arc::new(octos_agent::tools::ToolRegistry::new());
    let runner = ValidatorRunner::new(tools, workspace_dir.to_path_buf());
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: workspace_dir.to_path_buf(),
        repo_label: "swarm-subtask-test".into(),
    };
    AggregateValidator {
        runner,
        invocation,
        validators,
    }
}

// ── F-004 per-subtask validator gating ────────────────────────────────

#[tokio::test]
async fn should_run_completion_validators_in_swarm_subtask() {
    // The validator declares `required.txt` must exist (>= 1 byte).
    // The workspace is empty, so the validator MUST fail for every
    // subtask that returns Success — demoting them to TerminalFailed.
    let backend: Arc<dyn McpAgentBackend> = Arc::new(OkBackend);
    let workspace_dir = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();

    let aggregate = aggregate_validator(
        workspace_dir.path(),
        vec![required_file_validator("missing_artifact", "required.txt")],
    );

    let swarm = Swarm::builder(backend, state_dir.path())
        .with_validator(aggregate)
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d-subtask-fail",
            vec![contract("a"), contract("b")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(
        result.completed_subtasks, 0,
        "every subtask must be demoted to TerminalFailed by the per-subtask validator gate, but {} survived",
        result.completed_subtasks
    );
    assert!(
        matches!(
            result.outcome,
            SwarmOutcomeKind::Failed | SwarmOutcomeKind::Partial | SwarmOutcomeKind::Aborted
        ),
        "outcome must reflect validator rejection; got {:?}",
        result.outcome
    );
    for outcome in &result.per_task_outcomes {
        assert_eq!(outcome.status, octos_swarm::SubtaskStatus::TerminalFailed);
        let error = outcome
            .error
            .as_deref()
            .expect("per-subtask validator failure must populate SubtaskOutcome::error");
        assert!(
            error.contains("missing_artifact"),
            "error must reference the failing validator id; got `{error}`",
        );
    }
}

#[tokio::test]
async fn should_pass_swarm_subtask_when_required_validator_satisfied() {
    // Same fixture as the failing test, but we pre-write the artifact —
    // so every subtask passes both the backend dispatch AND the per-
    // subtask validator gate.
    let backend: Arc<dyn McpAgentBackend> = Arc::new(OkBackend);
    let workspace_dir = tempfile::tempdir().unwrap();
    std::fs::write(workspace_dir.path().join("required.txt"), b"ok").unwrap();
    let state_dir = tempfile::tempdir().unwrap();

    let aggregate = aggregate_validator(
        workspace_dir.path(),
        vec![required_file_validator("present_artifact", "required.txt")],
    );

    let swarm = Swarm::builder(backend, state_dir.path())
        .with_validator(aggregate)
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d-subtask-ok",
            vec![contract("a"), contract("b")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
    assert_eq!(result.completed_subtasks, 2);
    for outcome in &result.per_task_outcomes {
        assert_eq!(outcome.status, octos_swarm::SubtaskStatus::Completed);
    }
}

// ── F-004 per-subtask cost reservation ───────────────────────────────

#[tokio::test]
async fn should_use_reserve_api_for_budget_isolation_in_swarm() {
    // The budget policy caps the contract at a tiny dollar amount that
    // two concurrent pre-dispatch projections MUST exceed. With the
    // reservation wire-up from F-004, the second subtask observes the
    // first's in-flight reservation via the accountant's shared map
    // and is rejected before the backend is dialled. The pre-fix swarm
    // (with no reservation) would have admitted both — the test guards
    // that regression.
    //
    // We force both subtasks through a single contract_id budget by
    // passing `contract_id: "shared-cap"`. The per-subtask commit on
    // the surviving subtask then persists the projected cost through
    // the reservation handle, so a subsequent dispatch under the same
    // contract id sees the historical spend.

    let backend: Arc<dyn McpAgentBackend> = Arc::new(OkBackend);
    let ledger_dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(PersistentCostLedger::open(ledger_dir.path()).await.unwrap());

    // Tight $0.000015 cap against gpt-4o pricing ($2.50 per M input).
    // Each subtask's task JSON ({"contract_id":"x"}) is ~19 bytes →
    // tokens_in_estimate = 5 → projected_usd = 5 * 2.50 / 1_000_000 =
    // $0.0000125. The first reservation consumes $0.0000125, leaving
    // $0.0000025 headroom — less than any subsequent reservation, so
    // every concurrent sibling is rejected as a budget breach. A single
    // reserved/committed subtask survives. The SwarmCostBudget points
    // at a single contract id so all subtasks fold into the same
    // reservation bucket.
    let policy = CostBudgetPolicy::default().with_per_contract_usd(0.000015);
    let accountant = Arc::new(CostAccountant::new(ledger.clone(), Some(policy)));

    let state_dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend, state_dir.path())
        .with_cost_budget(SwarmCostBudget {
            accountant: accountant.clone(),
            model: "gpt-4o".to_string(),
            contract_id: "shared-cap".to_string(),
        })
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d-budget",
            vec![contract("a"), contract("b"), contract("c")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(3).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    // The budget MUST reject at least one subtask; the pre-fix swarm
    // would have admitted all three.
    let breached: Vec<&octos_swarm::SubtaskOutcome> = result
        .per_task_outcomes
        .iter()
        .filter(|outcome| {
            outcome.status == octos_swarm::SubtaskStatus::TerminalFailed
                && outcome.last_dispatch_outcome == "budget_breach"
        })
        .collect();
    assert!(
        !breached.is_empty(),
        "at least one subtask must be rejected by the cost-budget gate; got {:?}",
        result
            .per_task_outcomes
            .iter()
            .map(|o| (
                o.contract_id.clone(),
                o.status,
                o.last_dispatch_outcome.clone()
            ))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn should_commit_reservation_on_swarm_subtask_completion() {
    // A single subtask under a generous budget: the subtask MUST commit
    // through the reservation handle on the success path, persisting a
    // CostAttributionEvent to the underlying ledger. We verify this by
    // observing that the ledger has one row for the contract after
    // dispatch — the projection-only (reservation-only) behaviour would
    // leave the ledger empty.

    let backend: Arc<dyn McpAgentBackend> = Arc::new(OkBackend);
    let ledger_dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(PersistentCostLedger::open(ledger_dir.path()).await.unwrap());

    let policy = CostBudgetPolicy::default().with_per_contract_usd(100.0);
    let accountant = Arc::new(CostAccountant::new(ledger.clone(), Some(policy)));

    let state_dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend, state_dir.path())
        .with_cost_budget(SwarmCostBudget {
            accountant: accountant.clone(),
            model: "gpt-4o".to_string(),
            contract_id: "commit-test".to_string(),
        })
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d-commit",
            vec![contract("a")],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
    assert_eq!(result.completed_subtasks, 1);

    let rows = ledger.list_for_contract("commit-test").await.unwrap();
    assert!(
        !rows.is_empty(),
        "commit path must persist at least one CostAttributionEvent to the ledger; got none"
    );
}
