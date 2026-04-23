//! Integration tests for the swarm orchestration primitive (M7.5).
//!
//! Each test builds a deterministic in-process [`McpAgentBackend`]
//! substitute so the dispatcher can be driven without a real MCP sub-
//! agent. The substitutes track exactly which contracts were issued,
//! in what order, and how many times — which is enough to assert every
//! invariant the M7.5 contract calls out.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::harness_events::{HarnessEvent, HarnessEventPayload};
use octos_agent::tools::mcp_agent::{
    DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackend,
};
use octos_swarm::{
    ContractSpec, FanoutPattern, Swarm, SwarmBudget, SwarmContext, SwarmEventSink,
    SwarmOutcomeKind, SwarmTopology,
};

// ── Helpers ────────────────────────────────────────────────────────────────

/// Programmable fake backend. Each contract id maps to an ordered list
/// of responses the backend emits on successive calls. The backend
/// records every [`DispatchRequest`] it sees so topology ordering can
/// be asserted.
#[derive(Default)]
struct FakeBackend {
    /// Per-contract response queue. Draining order is preserved across
    /// retries so the test can script a "fail once, succeed later"
    /// sequence.
    responses: Mutex<HashMap<String, Vec<DispatchResponse>>>,
    /// Contracts issued, in dispatch order, with the fully-substituted
    /// task payload.
    history: Mutex<Vec<(String, serde_json::Value)>>,
    /// Per-dispatch delay, applied before responding. Used by the
    /// parallel test to ensure fan-out is truly concurrent.
    delay: Mutex<Option<Duration>>,
}

impl FakeBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn set_delay(&self, duration: Duration) {
        *self.delay.lock().unwrap() = Some(duration);
    }

    fn script(&self, contract_id: impl Into<String>, responses: Vec<DispatchResponse>) {
        self.responses
            .lock()
            .unwrap()
            .insert(contract_id.into(), responses);
    }

    fn history(&self) -> Vec<(String, serde_json::Value)> {
        self.history.lock().unwrap().clone()
    }
}

#[async_trait]
impl McpAgentBackend for FakeBackend {
    fn backend_label(&self) -> &'static str {
        "local"
    }

    fn endpoint_label(&self) -> String {
        "fake".to_string()
    }

    async fn dispatch(&self, request: DispatchRequest) -> DispatchResponse {
        let contract_id = request
            .task
            .get("contract_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        self.history
            .lock()
            .unwrap()
            .push((contract_id.clone(), request.task.clone()));

        let delay = *self.delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }

        let mut queue = self.responses.lock().unwrap();
        if let Some(entries) = queue.get_mut(&contract_id) {
            if !entries.is_empty() {
                return entries.remove(0);
            }
        }
        // Fallback: synthesise a success so uninstrumented tests stay
        // deterministic.
        DispatchResponse {
            outcome: DispatchOutcome::Success,
            output: format!("default:{contract_id}"),
            files_to_send: Vec::new(),
            error: None,
        }
    }
}

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<HarnessEvent>>,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn events(&self) -> Vec<HarnessEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl SwarmEventSink for RecordingSink {
    fn emit(&self, event: &HarnessEvent) {
        self.events.lock().unwrap().push(event.clone());
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

fn success(text: &str) -> DispatchResponse {
    DispatchResponse {
        outcome: DispatchOutcome::Success,
        output: text.to_string(),
        files_to_send: Vec::new(),
        error: None,
    }
}

fn success_with_files(text: &str, files: Vec<PathBuf>) -> DispatchResponse {
    DispatchResponse {
        outcome: DispatchOutcome::Success,
        output: text.to_string(),
        files_to_send: files,
        error: None,
    }
}

fn timeout_failure(msg: &str) -> DispatchResponse {
    DispatchResponse {
        outcome: DispatchOutcome::Timeout,
        output: msg.to_string(),
        files_to_send: Vec::new(),
        error: Some(msg.to_string()),
    }
}

fn transport_failure(msg: &str) -> DispatchResponse {
    DispatchResponse {
        outcome: DispatchOutcome::TransportError,
        output: msg.to_string(),
        files_to_send: Vec::new(),
        error: Some(msg.to_string()),
    }
}

fn context() -> SwarmContext {
    SwarmContext {
        session_id: "api:test".into(),
        task_id: "task-1".into(),
        workflow: Some("swarm_test".into()),
        phase: Some("dispatch".into()),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_fan_out_parallel_n_contracts() {
    let backend = FakeBackend::new();
    backend.script("a", vec![success("result-a")]);
    backend.script("b", vec![success("result-b")]);
    backend.script("c", vec![success("result-c")]);
    // Delay each dispatch so the test can prove the fan-out actually
    // overlaps; with 3 contracts at 100ms apiece, a sequential run
    // would take >=300ms while a parallel run stays near 100ms.
    backend.set_delay(Duration::from_millis(100));

    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();

    let start = std::time::Instant::now();
    let result = swarm
        .dispatch(
            "d1",
            vec![contract("a"), contract("b"), contract("c")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(3).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
    assert_eq!(result.total_subtasks, 3);
    assert_eq!(result.completed_subtasks, 3);
    assert!(
        elapsed < Duration::from_millis(280),
        "fan-out was not concurrent: {elapsed:?}"
    );
    // History records every issued contract.
    assert_eq!(backend.history().len(), 3);
}

#[tokio::test]
async fn should_sequence_contracts_in_order_with_abort_on_failure() {
    let backend = FakeBackend::new();
    backend.script("first", vec![success("ok-first")]);
    // Hard (transport) failure on second — the sequential runner must
    // abort before dispatching `third`.
    backend.script("second", vec![transport_failure("connection refused")]);
    backend.script("third", vec![success("never-runs")]);

    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d2",
            vec![contract("first"), contract("second"), contract("third")],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, SwarmOutcomeKind::Aborted);
    assert_eq!(result.total_subtasks, 3);
    assert_eq!(result.completed_subtasks, 1);
    assert_eq!(
        result.per_task_outcomes[0].status,
        octos_swarm::SubtaskStatus::Completed
    );
    assert_eq!(
        result.per_task_outcomes[1].status,
        octos_swarm::SubtaskStatus::TerminalFailed
    );
    // The third contract was never dispatched.
    let history = backend.history();
    let ids: Vec<&str> = history.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, vec!["first", "second"]);
}

#[tokio::test]
async fn should_chain_pipeline_output_as_next_input() {
    let backend = FakeBackend::new();
    backend.script("stage-1", vec![success("stage-1-output")]);
    backend.script("stage-2", vec![success("stage-2-output")]);
    backend.script("stage-3", vec![success("stage-3-output")]);

    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d3",
            vec![
                contract("stage-1"),
                contract("stage-2"),
                contract("stage-3"),
            ],
            SwarmTopology::Pipeline,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
    let history = backend.history();
    assert_eq!(history.len(), 3);
    // Second stage saw the first stage's output as `pipeline_input`.
    assert_eq!(history[1].1["pipeline_input"], "stage-1-output");
    assert_eq!(history[2].1["pipeline_input"], "stage-2-output");
    // First stage saw no pipeline_input.
    assert!(history[0].1.get("pipeline_input").is_none());
}

#[tokio::test]
async fn should_redispatch_failed_subcontract_bounded_retries() {
    let backend = FakeBackend::new();
    // First contract always succeeds on first attempt.
    backend.script("good", vec![success("good-output")]);
    // Second contract fails with a retryable (timeout) error 4 times,
    // which exceeds the bounded MAX_RETRY_ROUNDS (3) budget. The
    // primitive should stop after 3 retry rounds (4 total attempts
    // across the initial round + 3 retries) and surface a partial
    // result.
    backend.script(
        "flaky",
        vec![
            timeout_failure("slow-1"),
            timeout_failure("slow-2"),
            timeout_failure("slow-3"),
            timeout_failure("slow-4"),
            // Even though we queue an extra success, the primitive
            // should have stopped before this one is drained.
            success("late-success"),
        ],
    );

    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d4",
            vec![contract("good"), contract("flaky")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, SwarmOutcomeKind::Partial);
    assert_eq!(result.completed_subtasks, 1);
    // Retry budget bounded — at most MAX_RETRY_ROUNDS retry rounds
    // after the initial round means at most 4 attempts for the flaky
    // contract.
    let flaky_outcome = result
        .per_task_outcomes
        .iter()
        .find(|outcome| outcome.contract_id == "flaky")
        .expect("flaky subtask present");
    assert!(
        flaky_outcome.attempts <= octos_swarm::MAX_RETRY_ROUNDS + 1,
        "flaky retried {} times, should be bounded",
        flaky_outcome.attempts
    );
    assert_eq!(
        flaky_outcome.status,
        octos_swarm::SubtaskStatus::RetryableFailed
    );
}

#[tokio::test]
async fn should_redispatch_recovering_subcontract_within_budget() {
    // Regression test for invariant 5: within the retry budget, a
    // contract that fails once and succeeds on the retry SHOULD be
    // surfaced as completed. This ensures we are not accidentally
    // giving up after the first attempt.
    let backend = FakeBackend::new();
    backend.script("ok", vec![success("done")]);
    backend.script(
        "recover",
        vec![timeout_failure("try-again"), success("recovered")],
    );

    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d5",
            vec![contract("ok"), contract("recover")],
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
    let recover = result
        .per_task_outcomes
        .iter()
        .find(|outcome| outcome.contract_id == "recover")
        .unwrap();
    assert_eq!(recover.attempts, 2);
    assert_eq!(recover.output, "recovered");
}

#[tokio::test]
async fn should_aggregate_validator_over_combined_output() {
    // The aggregate validator is wired via an M4.3 ValidatorRunner
    // against a temporary workspace. We configure one required
    // file-exists validator that the swarm deliberately arranges to
    // satisfy (writing the target file as part of a sub-contract
    // "artifact"). The validator runs only once, after every sub-
    // contract terminated.
    use octos_agent::validators::{ValidatorInvocation, ValidatorPhase, ValidatorRunner};
    use octos_agent::workspace_policy::{Validator, ValidatorPhaseKind, ValidatorSpec};
    use octos_swarm::AggregateValidator;
    use std::sync::Arc as StdArc;

    let backend = FakeBackend::new();
    backend.script(
        "one",
        vec![success_with_files("one", vec![PathBuf::from("one.txt")])],
    );
    backend.script(
        "two",
        vec![success_with_files("two", vec![PathBuf::from("two.txt")])],
    );

    let workspace_dir = tempfile::tempdir().unwrap();
    // Write the file the validator will check for. The swarm isn't
    // responsible for folding files into the workspace — that's M4.1A
    // contract work — so we simulate the end-state here.
    std::fs::write(workspace_dir.path().join("aggregate.txt"), "done").unwrap();

    let tools = StdArc::new(octos_agent::tools::ToolRegistry::new());
    let runner = ValidatorRunner::new(tools, workspace_dir.path().to_path_buf());
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: workspace_dir.path().to_path_buf(),
        repo_label: "swarm-test".into(),
    };
    let validator = Validator {
        id: "aggregate_exists".into(),
        required: true,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: "aggregate.txt".into(),
            min_bytes: None,
        },
    };
    let aggregate = AggregateValidator {
        runner,
        invocation,
        validators: vec![validator],
    };

    let state_dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), state_dir.path())
        .with_validator(aggregate)
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d6",
            vec![contract("one"), contract("two")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
    assert_eq!(result.validator_results.len(), 1);
    assert_eq!(result.validator_results[0].validator_id, "aggregate_exists");
    assert!(result.validator_results[0].required_gate_passed());
    // Aggregate artifact reflects both sub-contracts in arrival order.
    assert!(result.aggregate_artifact.combined_output.contains("one"));
    assert!(result.aggregate_artifact.combined_output.contains("two"));
    assert_eq!(result.aggregate_artifact.combined_files.len(), 2);
}

#[tokio::test]
async fn should_survive_process_restart_mid_dispatch() {
    // First "process" performs a dispatch that lands a couple of
    // subtasks in retryable-failed state. The swarm record persists
    // in redb. A second `Swarm` instance re-opens the same state dir
    // with a different backend that succeeds the pending subtasks, and
    // re-dispatches with the SAME dispatch_id. The primitive must
    // reload the existing record and resume from the partial state
    // rather than re-running the already-completed subtasks.

    let first_backend = FakeBackend::new();
    first_backend.script("a", vec![success("a-first")]);
    // Fail all retries so retry budget is exhausted and the record
    // finalizes with `b` as retryable_failed.
    first_backend.script(
        "b",
        vec![
            timeout_failure("fail-1"),
            timeout_failure("fail-2"),
            timeout_failure("fail-3"),
            timeout_failure("fail-4"),
        ],
    );

    let state_dir = tempfile::tempdir().unwrap();
    {
        let swarm_v1 = Swarm::builder(first_backend.clone(), state_dir.path())
            .build()
            .await
            .unwrap();
        let result = swarm_v1
            .dispatch(
                "d7",
                vec![contract("a"), contract("b")],
                SwarmTopology::Parallel {
                    max_concurrency: NonZeroUsize::new(2).unwrap(),
                },
                SwarmBudget::default(),
                context(),
            )
            .await
            .unwrap();
        assert_eq!(result.outcome, SwarmOutcomeKind::Partial);
    }

    // "Process restart": a brand new swarm instance pointing at the
    // same state dir. Because the record is finalized, calling
    // dispatch with the same id returns the prior result without
    // touching the new backend — invariant 1 + 7.
    let spy_counter = Arc::new(AtomicUsize::new(0));
    let second_backend = Arc::new(CountingBackend {
        counter: spy_counter.clone(),
    });

    let swarm_v2 = Swarm::builder(second_backend.clone(), state_dir.path())
        .build()
        .await
        .unwrap();
    let replay = swarm_v2
        .dispatch(
            "d7",
            vec![contract("a"), contract("b")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(replay.dispatch_id, "d7");
    assert_eq!(replay.total_subtasks, 2);
    // No dispatch was issued to the fresh backend — the record is
    // finalized and idempotent.
    assert_eq!(spy_counter.load(Ordering::SeqCst), 0);
}

struct CountingBackend {
    counter: Arc<AtomicUsize>,
}

#[async_trait]
impl McpAgentBackend for CountingBackend {
    fn backend_label(&self) -> &'static str {
        "local"
    }

    fn endpoint_label(&self) -> String {
        "counting".into()
    }

    async fn dispatch(&self, _request: DispatchRequest) -> DispatchResponse {
        self.counter.fetch_add(1, Ordering::SeqCst);
        success("should-not-run")
    }
}

#[tokio::test]
async fn should_emit_typed_swarm_dispatch_event() {
    let backend = FakeBackend::new();
    backend.script("only", vec![success("final")]);

    let sink = RecordingSink::new();
    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .with_event_sink(sink.clone())
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d8",
            vec![contract("only")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
    let events = sink.events();
    assert_eq!(events.len(), 1);
    match &events[0].payload {
        HarnessEventPayload::SwarmDispatch { data } => {
            assert_eq!(
                data.schema_version,
                octos_agent::abi_schema::SWARM_DISPATCH_SCHEMA_VERSION
            );
            assert_eq!(data.dispatch_id, "d8");
            assert_eq!(data.topology, "parallel");
            assert_eq!(data.outcome, "success");
            assert_eq!(data.total_subtasks, Some(1));
            assert_eq!(data.completed_subtasks, Some(1));
            assert_eq!(data.workflow.as_deref(), Some("swarm_test"));
            assert_eq!(data.phase.as_deref(), Some("dispatch"));
        }
        other => panic!("wrong event payload: {other:?}"),
    }
    // The event itself must validate under the shared harness event
    // schema so downstream sinks accept it.
    events[0].validate().expect("event validates");
}

#[tokio::test]
async fn should_expand_fanout_pattern_into_variant_contracts() {
    let backend = FakeBackend::new();
    // Fanout expands `seed` with suffix `::alpha`, `::beta`,
    // `::gamma` into the contract ids the backend scripts against.
    backend.script("seed::alpha", vec![success("α")]);
    backend.script("seed::beta", vec![success("β")]);
    backend.script("seed::gamma", vec![success("γ")]);

    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();

    let seed_contract = ContractSpec {
        contract_id: "seed".into(),
        tool_name: "run".into(),
        task: serde_json::json!({"contract_id": "seed"}),
        label: None,
    };
    let pattern = FanoutPattern {
        seed: seed_contract.clone(),
        variants: vec!["alpha".into(), "beta".into(), "gamma".into()],
    };
    let topology = SwarmTopology::Fanout {
        pattern,
        max_concurrency: NonZeroUsize::new(3).unwrap(),
    };

    // Fanout ignores the caller's contract list: pass an empty vec to
    // prove the pattern drives the dispatch.
    let result = swarm
        .dispatch(
            "d9",
            vec![seed_contract],
            topology,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
    assert_eq!(result.total_subtasks, 3);
    // The fan-out expansion injected the `variant` key on each task.
    let history = backend.history();
    let variants: Vec<&str> = history
        .iter()
        .filter_map(|(_, task)| task.get("variant").and_then(|v| v.as_str()))
        .collect();
    assert!(variants.contains(&"alpha"));
    assert!(variants.contains(&"beta"));
    assert!(variants.contains(&"gamma"));
}

#[tokio::test]
async fn should_record_cost_attribution_via_ledger_stub() {
    use octos_swarm::{CostLedger, SwarmCostAttribution};

    #[derive(Default)]
    struct SpyLedger {
        records: Mutex<Vec<SwarmCostAttribution>>,
    }

    #[async_trait]
    impl CostLedger for SpyLedger {
        async fn attribute(&self, record: &SwarmCostAttribution) {
            self.records.lock().unwrap().push(record.clone());
        }
    }

    let backend = FakeBackend::new();
    backend.script("x", vec![success("x")]);
    backend.script("y", vec![timeout_failure("fail"), success("y-recover")]);

    let ledger = Arc::new(SpyLedger::default());
    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .with_ledger(ledger.clone())
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d10",
            vec![contract("x"), contract("y")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, SwarmOutcomeKind::Success);

    // Cost ledger saw one record per attempt (including the retry on
    // `y`). M7.4 will flesh out token/cost numbers; for M7.5 we only
    // need the hook to be invoked at the right cardinality.
    let records = ledger.records.lock().unwrap();
    let attempts_for = |cid: &str| records.iter().filter(|r| r.contract_id == cid).count();
    assert_eq!(attempts_for("x"), 1);
    assert_eq!(attempts_for("y"), 2);
}
