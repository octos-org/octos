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
    /// Number of backend dispatches currently inside the fake backend.
    active_dispatches: AtomicUsize,
    /// High-water mark for simultaneously active dispatches.
    max_active_dispatches: AtomicUsize,
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

    fn max_active_dispatches(&self) -> usize {
        self.max_active_dispatches.load(Ordering::SeqCst)
    }
}

struct ActiveDispatchGuard<'a> {
    backend: &'a FakeBackend,
}

impl<'a> ActiveDispatchGuard<'a> {
    fn enter(backend: &'a FakeBackend) -> Self {
        let active = backend.active_dispatches.fetch_add(1, Ordering::SeqCst) + 1;
        backend
            .max_active_dispatches
            .fetch_max(active, Ordering::SeqCst);
        Self { backend }
    }
}

impl Drop for ActiveDispatchGuard<'_> {
    fn drop(&mut self) {
        self.backend
            .active_dispatches
            .fetch_sub(1, Ordering::SeqCst);
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

        let _active_dispatch = ActiveDispatchGuard::enter(self);
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
            context_contract: None,
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
        context_contract: None,
    }
}

fn success_with_files(text: &str, files: Vec<PathBuf>) -> DispatchResponse {
    DispatchResponse {
        outcome: DispatchOutcome::Success,
        output: text.to_string(),
        files_to_send: files,
        error: None,
        context_contract: None,
    }
}

fn timeout_failure(msg: &str) -> DispatchResponse {
    DispatchResponse {
        outcome: DispatchOutcome::Timeout,
        output: msg.to_string(),
        files_to_send: Vec::new(),
        error: Some(msg.to_string()),
        context_contract: None,
    }
}

fn transport_failure(msg: &str) -> DispatchResponse {
    DispatchResponse {
        outcome: DispatchOutcome::TransportError,
        output: msg.to_string(),
        files_to_send: Vec::new(),
        error: Some(msg.to_string()),
        context_contract: None,
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

/// Hand-built subtask row for tests that seed the redb ledger with a
/// mid-dispatch (non-finalized) record.
fn seeded_subtask(
    id: &str,
    status: octos_swarm::SubtaskStatus,
    last_outcome: &str,
    output: &str,
) -> octos_swarm::SubtaskOutcome {
    octos_swarm::SubtaskOutcome {
        contract_id: id.into(),
        label: None,
        status,
        attempts: if last_outcome == "not_run" { 0 } else { 1 },
        last_dispatch_outcome: last_outcome.into(),
        output: output.into(),
        files_to_send: Vec::new(),
        error: None,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_fan_out_parallel_n_contracts() {
    let backend = FakeBackend::new();
    backend.script("a", vec![success("result-a")]);
    backend.script("b", vec![success("result-b")]);
    backend.script("c", vec![success("result-c")]);
    // Delay each dispatch so the test can prove fan-out overlap using
    // the fake backend's active-dispatch high-water mark.
    backend.set_delay(Duration::from_millis(100));

    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();

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

    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
    assert_eq!(result.total_subtasks, 3);
    assert_eq!(result.completed_subtasks, 3);
    assert!(
        backend.max_active_dispatches() > 1,
        "fan-out did not overlap dispatches; max active dispatches: {}",
        backend.max_active_dispatches()
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
        input_args: None,
        tool_output: None,
        spawn_only_files: Vec::new(),
    };
    let validator = Validator {
        id: "aggregate_exists".into(),
        required: true,
        soft_fail: false,
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
async fn should_stop_pipeline_round_at_retryable_stage_and_resume_with_input() {
    // #1717: a retryable mid-pipeline failure must end the round. With
    // the old behaviour stage-3 dispatched in the same round with NO
    // `pipeline_input` (stage-2 had not completed) and was marked
    // Completed against a silently-broken chain.
    let backend = FakeBackend::new();
    backend.script("stage-1", vec![success("s1-out")]);
    backend.script(
        "stage-2",
        vec![timeout_failure("s2-flaky"), success("s2-out")],
    );
    backend.script("stage-3", vec![success("s3-out")]);

    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();

    let result = swarm
        .dispatch(
            "d11",
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
    // stage-2 was attempted twice, both times chained to stage-1.
    let stage_2: Vec<_> = history.iter().filter(|(id, _)| id == "stage-2").collect();
    assert_eq!(stage_2.len(), 2);
    for (_, task) in &stage_2 {
        assert_eq!(task["pipeline_input"], "s1-out");
    }
    // stage-3 ran exactly once — AFTER stage-2 recovered — and saw its
    // output. The old behaviour dispatched it early with no input key.
    let stage_3: Vec<_> = history.iter().filter(|(id, _)| id == "stage-3").collect();
    assert_eq!(
        stage_3.len(),
        1,
        "stage-3 must not run before stage-2 completes"
    );
    assert_eq!(
        stage_3[0].1.get("pipeline_input").and_then(|v| v.as_str()),
        Some("s2-out"),
        "stage-3 must chain the recovered stage-2 output"
    );
}

#[tokio::test]
async fn should_replay_finalized_result_verbatim_including_validator_verdicts() {
    // #1718: replaying a finalized dispatch must return the ORIGINAL
    // computed result — validator verdicts and outcome included. The
    // old short-circuit recomputed from subtask state with empty
    // validator results, upgrading a validator-failed Partial to
    // Success on re-POST.
    use octos_agent::validators::{ValidatorInvocation, ValidatorPhase, ValidatorRunner};
    use octos_agent::workspace_policy::{Validator, ValidatorPhaseKind, ValidatorSpec};
    use octos_swarm::AggregateValidator;
    use std::sync::Arc as StdArc;

    let backend = FakeBackend::new();
    backend.script("only", vec![success("payload")]);

    let workspace_dir = tempfile::tempdir().unwrap();
    // Deliberately do NOT create the file the validator requires — the
    // aggregate validator must fail and demote the outcome.
    let tools = StdArc::new(octos_agent::tools::ToolRegistry::new());
    let runner = ValidatorRunner::new(tools, workspace_dir.path().to_path_buf());
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: workspace_dir.path().to_path_buf(),
        repo_label: "swarm-replay-test".into(),
        input_args: None,
        tool_output: None,
        spawn_only_files: Vec::new(),
    };
    let validator = Validator {
        id: "missing_artifact".into(),
        required: true,
        soft_fail: false,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: "never-written.txt".into(),
            min_bytes: None,
        },
    };
    let aggregate = AggregateValidator {
        runner,
        invocation,
        validators: vec![validator],
    };

    let state_dir = tempfile::tempdir().unwrap();
    let original = {
        let swarm = Swarm::builder(backend.clone(), state_dir.path())
            .with_validator(aggregate)
            .build()
            .await
            .unwrap();
        swarm
            .dispatch(
                "d12",
                vec![contract("only")],
                SwarmTopology::Sequential,
                SwarmBudget::default(),
                context(),
            )
            .await
            .unwrap()
    };
    // Required validator failed → the subtask is demoted, so the
    // original result must NOT be Success and must carry the verdict.
    assert_ne!(original.outcome, SwarmOutcomeKind::Success);
    assert!(
        !original.validator_results.is_empty()
            || original
                .per_task_outcomes
                .iter()
                .any(|o| o.status != octos_swarm::SubtaskStatus::Completed),
        "validator failure must be visible in the original result"
    );

    // Replay on a fresh swarm (no validator configured, counting
    // backend): must return the stored result verbatim, not recompute.
    let spy_counter = Arc::new(AtomicUsize::new(0));
    let second_backend = Arc::new(CountingBackend {
        counter: spy_counter.clone(),
    });
    let swarm_v2 = Swarm::builder(second_backend, state_dir.path())
        .build()
        .await
        .unwrap();
    let replay = swarm_v2
        .dispatch(
            "d12",
            vec![contract("only")],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(spy_counter.load(Ordering::SeqCst), 0);
    assert_eq!(
        replay, original,
        "finalized replay must return the original result verbatim"
    );
}

#[tokio::test]
async fn should_error_not_panic_when_resume_contracts_fewer_than_recorded() {
    // #1719: a non-finalized record with MORE subtasks than the caller's
    // contract list used to drive `contracts[idx]` out of bounds — a
    // panic in production. It must surface as a typed error instead.
    use octos_swarm::{DispatchRecord, DispatchStore, SubtaskOutcome, SubtaskStatus};

    let state_dir = tempfile::tempdir().unwrap();
    let pending = |id: &str| SubtaskOutcome {
        contract_id: id.into(),
        label: None,
        status: SubtaskStatus::RetryableFailed,
        attempts: 0,
        last_dispatch_outcome: "not_run".into(),
        output: String::new(),
        files_to_send: Vec::new(),
        error: None,
    };
    {
        let store = DispatchStore::open(state_dir.path()).await.unwrap();
        let record = DispatchRecord::new(
            "d13",
            "api:test",
            "task-1",
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
            },
            vec![pending("a"), pending("b"), pending("c")],
        );
        store.store(&record).await.unwrap();
    }

    let backend = FakeBackend::new();
    let swarm = Swarm::builder(backend.clone(), state_dir.path())
        .build()
        .await
        .unwrap();
    let error = swarm
        .dispatch(
            "d13",
            vec![contract("a"), contract("b")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .expect_err("mismatched resume must error, not panic");
    assert!(
        error.to_string().contains("d13"),
        "error should name the dispatch id: {error}"
    );
    // The in-flight guard must be released on the error path — for the
    // SAME id, not just fresh ones: d13 with the correct recorded shape
    // must now resume cleanly (review finding 6 on the original test).
    let resumed = swarm
        .dispatch(
            "d13",
            vec![contract("a"), contract("b"), contract("c")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .expect("guard must be released for the failed id itself");
    assert_eq!(resumed.outcome, SwarmOutcomeKind::Success);
    let ok = swarm
        .dispatch(
            "d13-fresh",
            vec![contract("a")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(ok.outcome, SwarmOutcomeKind::Success);
}

#[tokio::test]
async fn should_error_when_resume_contract_ids_mismatch_recorded() {
    // #1719: same count but different contract ids — silently retrying
    // slot N against a DIFFERENT contract would attribute outputs to
    // the wrong contract. Must be rejected.
    use octos_swarm::{DispatchRecord, DispatchStore, SubtaskOutcome, SubtaskStatus};

    let state_dir = tempfile::tempdir().unwrap();
    {
        let store = DispatchStore::open(state_dir.path()).await.unwrap();
        let record = DispatchRecord::new(
            "d14",
            "api:test",
            "task-1",
            SwarmTopology::Sequential,
            vec![SubtaskOutcome {
                contract_id: "original".into(),
                label: None,
                status: SubtaskStatus::RetryableFailed,
                attempts: 0,
                last_dispatch_outcome: "not_run".into(),
                output: String::new(),
                files_to_send: Vec::new(),
                error: None,
            }],
        );
        store.store(&record).await.unwrap();
    }

    let backend = FakeBackend::new();
    let swarm = Swarm::builder(backend.clone(), state_dir.path())
        .build()
        .await
        .unwrap();
    let error = swarm
        .dispatch(
            "d14",
            vec![contract("different")],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .expect_err("contract id mismatch must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("original") && message.contains("different"),
        "error should name both contract ids: {message}"
    );
    assert!(backend.history().is_empty(), "backend must not be touched");
}

#[tokio::test]
async fn should_error_when_finalized_replay_contracts_mismatch() {
    // #1719: a finalized record replayed with a DIFFERENT contract list
    // used to silently return the old result — the caller believes the
    // new contracts ran. Must be rejected instead.
    let backend = FakeBackend::new();
    backend.script("a", vec![success("a-out")]);

    let state_dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), state_dir.path())
        .build()
        .await
        .unwrap();
    swarm
        .dispatch(
            "d15",
            vec![contract("a")],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    let error = swarm
        .dispatch(
            "d15",
            vec![contract("a"), contract("b")],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .expect_err("finalized replay with different contracts must be rejected");
    assert!(
        error.to_string().contains("d15"),
        "error should name the dispatch id: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_reject_concurrent_dispatch_with_same_id() {
    // #1719: two concurrent dispatches with the same id used to race
    // the load/store pair — double-dispatching subtasks and clobbering
    // each other's rounds. The second caller must be rejected while the
    // first is in flight, and the id must be usable again afterwards.
    let backend = FakeBackend::new();
    backend.script("slow", vec![success("slow-out")]);
    backend.set_delay(Duration::from_millis(200));

    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();

    let contracts = || vec![contract("slow")];
    let topology = || SwarmTopology::Parallel {
        max_concurrency: NonZeroUsize::new(1).unwrap(),
    };
    let (first, second) = tokio::join!(
        swarm.dispatch(
            "d16",
            contracts(),
            topology(),
            SwarmBudget::default(),
            context()
        ),
        swarm.dispatch(
            "d16",
            contracts(),
            topology(),
            SwarmBudget::default(),
            context()
        ),
    );

    let error = match (first, second) {
        (Ok(_), Err(error)) | (Err(error), Ok(_)) => error,
        (Ok(_), Ok(_)) => panic!("both concurrent dispatches succeeded"),
        (Err(first), Err(second)) => panic!("both dispatches failed: {first}; {second}"),
    };
    assert!(
        error.to_string().contains("in flight"),
        "loser should be told the id is in flight: {error}"
    );
    // Only one dispatch reached the backend.
    assert_eq!(backend.history().len(), 1);

    // Guard released: the same id now replays the finalized result.
    let replay = swarm
        .dispatch(
            "d16",
            contracts(),
            topology(),
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(replay.outcome, SwarmOutcomeKind::Success);
}

#[tokio::test]
async fn should_not_resume_pipeline_tail_after_persisted_terminal_failure() {
    // Review finding 1 on #1717: a crash/cancel can persist a
    // non-finalized record whose mid-pipeline stage failed TERMINALLY.
    // On resume the terminal stage is no longer pending, so the old
    // code dispatched the tail with no `pipeline_input` and rolled the
    // outcome up as Partial instead of Aborted.
    use octos_swarm::{DispatchRecord, DispatchStore, SubtaskStatus};

    let state_dir = tempfile::tempdir().unwrap();
    {
        let store = DispatchStore::open(state_dir.path()).await.unwrap();
        let record = DispatchRecord::new(
            "d20",
            "api:test",
            "task-1",
            SwarmTopology::Pipeline,
            vec![
                seeded_subtask("a", SubtaskStatus::Completed, "success", "a-out"),
                seeded_subtask("b", SubtaskStatus::TerminalFailed, "transport_error", ""),
                seeded_subtask("c", SubtaskStatus::RetryableFailed, "not_run", ""),
            ],
        );
        store.store(&record).await.unwrap();
    }

    let backend = FakeBackend::new();
    let swarm = Swarm::builder(backend.clone(), state_dir.path())
        .build()
        .await
        .unwrap();
    let result = swarm
        .dispatch(
            "d20",
            vec![contract("a"), contract("b"), contract("c")],
            SwarmTopology::Pipeline,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert!(
        backend.history().is_empty(),
        "tail stage must not dispatch behind a terminal failure: {:?}",
        backend.history()
    );
    assert_eq!(result.outcome, SwarmOutcomeKind::Aborted);
}

#[tokio::test]
async fn should_not_resume_sequential_tail_after_persisted_terminal_failure() {
    // Same resume hole for Sequential: invariant 3 says the dispatch
    // aborted at the first terminal failure — a resumed record must not
    // dispatch the contracts behind it.
    use octos_swarm::{DispatchRecord, DispatchStore, SubtaskStatus};

    let state_dir = tempfile::tempdir().unwrap();
    {
        let store = DispatchStore::open(state_dir.path()).await.unwrap();
        let record = DispatchRecord::new(
            "d21",
            "api:test",
            "task-1",
            SwarmTopology::Sequential,
            vec![
                seeded_subtask("a", SubtaskStatus::TerminalFailed, "transport_error", ""),
                seeded_subtask("b", SubtaskStatus::RetryableFailed, "not_run", ""),
            ],
        );
        store.store(&record).await.unwrap();
    }

    let backend = FakeBackend::new();
    let swarm = Swarm::builder(backend.clone(), state_dir.path())
        .build()
        .await
        .unwrap();
    let result = swarm
        .dispatch(
            "d21",
            vec![contract("a"), contract("b")],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert!(
        backend.history().is_empty(),
        "aborted tail must stay not_run"
    );
    assert_eq!(result.outcome, SwarmOutcomeKind::Aborted);
}

#[tokio::test]
async fn should_not_run_extra_round_when_resumed_at_retry_cap() {
    // Review finding 2: the cap was checked only AFTER a round ran, so
    // a record checkpointed at the cap got one extra round per resume —
    // repeated crash/resume cycles made the bounded-cost invariant
    // unbounded.
    use octos_swarm::{DispatchRecord, DispatchStore, SubtaskStatus};

    let state_dir = tempfile::tempdir().unwrap();
    {
        let store = DispatchStore::open(state_dir.path()).await.unwrap();
        let mut record = DispatchRecord::new(
            "d22",
            "api:test",
            "task-1",
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            vec![seeded_subtask(
                "a",
                SubtaskStatus::RetryableFailed,
                "timeout",
                "",
            )],
        );
        record.retry_rounds_used = octos_swarm::MAX_RETRY_ROUNDS;
        store.store(&record).await.unwrap();
    }

    let backend = FakeBackend::new();
    let swarm = Swarm::builder(backend.clone(), state_dir.path())
        .build()
        .await
        .unwrap();
    let result = swarm
        .dispatch(
            "d22",
            vec![contract("a")],
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert!(
        backend.history().is_empty(),
        "a record resumed at the retry cap must not dispatch again"
    );
    assert_eq!(result.retry_rounds_used, octos_swarm::MAX_RETRY_ROUNDS);
    assert_eq!(result.outcome, SwarmOutcomeKind::Failed);
}

#[tokio::test]
async fn should_recompute_replay_for_legacy_finalized_record_without_snapshot() {
    // #1718 back-compat: rows persisted before `final_result` existed
    // must still replay via the legacy recomputation fallback.
    use octos_swarm::{DispatchRecord, DispatchStore, SubtaskStatus};

    let state_dir = tempfile::tempdir().unwrap();
    {
        let store = DispatchStore::open(state_dir.path()).await.unwrap();
        let mut record = DispatchRecord::new(
            "d23",
            "api:test",
            "task-1",
            SwarmTopology::Sequential,
            vec![seeded_subtask(
                "a",
                SubtaskStatus::Completed,
                "success",
                "legacy-out",
            )],
        );
        record.finalized = true;
        assert!(
            record.final_result.is_none(),
            "legacy row must lack snapshot"
        );
        store.store(&record).await.unwrap();
    }

    let backend = FakeBackend::new();
    let swarm = Swarm::builder(backend.clone(), state_dir.path())
        .build()
        .await
        .unwrap();
    let replay = swarm
        .dispatch(
            "d23",
            vec![contract("a")],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert!(backend.history().is_empty());
    assert_eq!(replay.outcome, SwarmOutcomeKind::Success);
    assert_eq!(replay.aggregate_artifact.combined_output, "legacy-out");
}

#[tokio::test]
async fn should_reject_same_id_with_changed_task_payload() {
    // Review finding 4: id-equality alone let a re-POST with the same
    // contract_id but a DIFFERENT task payload silently replay stale
    // results. The recorded payload fingerprint must reject it.
    let backend = FakeBackend::new();
    backend.script("a", vec![success("a-out")]);

    let state_dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), state_dir.path())
        .build()
        .await
        .unwrap();
    swarm
        .dispatch(
            "d24",
            vec![contract("a")],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    let mut changed = contract("a");
    changed.task = serde_json::json!({ "contract_id": "a", "extra": "changed" });
    let error = swarm
        .dispatch(
            "d24",
            vec![changed],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .expect_err("changed payload under a reused dispatch_id must be rejected");
    assert!(
        error.to_string().contains("payload"),
        "error should name the payload mismatch: {error}"
    );
}

#[tokio::test]
async fn should_not_burn_retry_budget_on_progress_rounds() {
    // Review finding 5: with #1717's early stop, a first-time failure
    // at each successive pipeline stage consumed a whole global round —
    // a 4-stage pipeline where every stage recovers on its second try
    // exhausted the cap before stage 4 got its retry. Rounds that
    // complete at least one new subtask must not consume the budget.
    let backend = FakeBackend::new();
    backend.script("s1", vec![success("s1-out")]);
    backend.script("s2", vec![timeout_failure("s2-flaky"), success("s2-out")]);
    backend.script("s3", vec![timeout_failure("s3-flaky"), success("s3-out")]);
    backend.script("s4", vec![timeout_failure("s4-flaky"), success("s4-out")]);

    let dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend.clone(), dir.path())
        .build()
        .await
        .unwrap();
    let result = swarm
        .dispatch(
            "d25",
            vec![
                contract("s1"),
                contract("s2"),
                contract("s3"),
                contract("s4"),
            ],
            SwarmTopology::Pipeline,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(
        result.outcome,
        SwarmOutcomeKind::Success,
        "every stage recovers within one retry — progress rounds must not starve stage 4"
    );
    assert_eq!(result.completed_subtasks, 4);
    // Chaining stayed intact throughout the retries.
    let history = backend.history();
    let s4_inputs: Vec<_> = history
        .iter()
        .filter(|(id, _)| id == "s4")
        .map(|(_, task)| task.get("pipeline_input").cloned())
        .collect();
    assert!(
        s4_inputs
            .iter()
            .all(|input| input.as_ref().and_then(|v| v.as_str()) == Some("s3-out")),
        "every s4 attempt must chain s3's output: {s4_inputs:?}"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn should_dispatch_through_cli_backend_with_retry_on_nonzero_exit() {
    // The CLI backend lane end-to-end through the real dispatcher: a
    // one-shot script that fails on its first invocation (non-zero
    // exit → RemoteError → retryable) and echoes the prompt on the
    // retry. Proves exit-code → outcome classification drives the
    // swarm retry loop exactly like MCP isError does.
    use octos_agent::tools::mcp_agent::{CliAgentBackend, McpAgentBackendConfig};

    let script_dir = tempfile::tempdir().unwrap();
    let marker = script_dir.path().join("attempted");
    let script_path = script_dir.path().join("flaky-cli.sh");
    std::fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nif [ ! -f {marker} ]; then\n  touch {marker}\n  echo 'transient' >&2\n  exit 1\nfi\nprintf 'cli-answer:%s' \"$1\"\n",
            marker = marker.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    let backend = Arc::new(
        CliAgentBackend::from_config(&McpAgentBackendConfig::Cli {
            cmd: script_path.display().to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            dispatch_timeout_secs: Some(10),
            prompt_via_stdin: false,
        })
        .unwrap(),
    );

    let state_dir = tempfile::tempdir().unwrap();
    let swarm = Swarm::builder(backend, state_dir.path())
        .build()
        .await
        .unwrap();
    let result = swarm
        .dispatch(
            "d-cli",
            vec![ContractSpec {
                contract_id: "c1".into(),
                tool_name: "ignored".into(),
                task: serde_json::json!({ "prompt": "do the thing" }),
                label: None,
            }],
            SwarmTopology::Sequential,
            SwarmBudget::default(),
            context(),
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, SwarmOutcomeKind::Success);
    let subtask = &result.per_task_outcomes[0];
    assert_eq!(subtask.attempts, 2, "first attempt fails, retry succeeds");
    assert_eq!(subtask.output, "cli-answer:do the thing");
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
