//! Ready-set DAG scheduler tests.
//!
//! Each test builds a graph and runs it on the DAG scheduler via
//! `PipelineExecutor::with_dag_scheduler(true)` with a `RecordingHandler`
//! (no real LLM), then asserts the traversal semantics the single-path walk
//! could not provide: diamond join, conditional routing, fail-closed pruning,
//! and bounded retry loops. A contrast test pins the latent single-path bug.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_core::TokenUsage;
use octos_memory::EpisodeStore;
use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor, PipelineResult};
use octos_pipeline::graph::{HandlerKind, NodeOutcome, OutcomeStatus, PipelineNode};
use octos_pipeline::handler::{Handler, HandlerContext, HandlerRegistry};
use tempfile::TempDir;

// --- Stub provider: never called (RecordingHandler replaces dispatch). ---
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

/// `(node_id, 1-based call index) -> status` — lets a test make a specific
/// node Fail (once or always) to exercise routing/pruning/retry.
type Decider = Arc<dyn Fn(&str, u32) -> OutcomeStatus + Send + Sync>;

#[derive(Default)]
struct ExecState {
    /// Node ids in execution order (re-runs append again).
    order: Vec<String>,
    /// Last-run predecessor-outcome count per node (the join width).
    preds_seen: HashMap<String, usize>,
    /// Total executions per node.
    calls: HashMap<String, u32>,
    /// Last-run task input per node (used to assert feedback propagation).
    inputs: HashMap<String, String>,
}

struct RecordingHandler {
    state: Arc<Mutex<ExecState>>,
    decide: Decider,
}

#[async_trait]
impl Handler for RecordingHandler {
    async fn execute(
        &self,
        node: &PipelineNode,
        ctx: &HandlerContext,
    ) -> eyre::Result<NodeOutcome> {
        let call = {
            let mut st = self.state.lock().unwrap();
            let c = st.calls.entry(node.id.clone()).or_insert(0);
            *c += 1;
            let call = *c;
            st.order.push(node.id.clone());
            st.preds_seen
                .insert(node.id.clone(), ctx.predecessor_outcomes.len());
            st.inputs.insert(node.id.clone(), ctx.input.clone());
            call
        };
        let status = (self.decide)(&node.id, call);
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status,
            content: format!("{}#{call}", node.id),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        })
    }
}

fn pass_all() -> Decider {
    Arc::new(|_n, _c| OutcomeStatus::Pass)
}

async fn run_with(
    dot: &str,
    decide: Decider,
    dag: bool,
) -> (PipelineResult, Arc<Mutex<ExecState>>) {
    let dir = TempDir::new().unwrap();
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let config = ExecutorConfig {
        guards: Vec::new(),
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
    };
    let exec = PipelineExecutor::new(config).with_dag_scheduler(dag);
    let state = Arc::new(Mutex::new(ExecState::default()));
    let handler = Arc::new(RecordingHandler {
        state: state.clone(),
        decide,
    });
    let mut handlers = HandlerRegistry::new();
    handlers.register(HandlerKind::Codergen, handler.clone());
    handlers.register(HandlerKind::Gate, handler.clone());
    handlers.register(HandlerKind::Shell, handler.clone());
    handlers.register(HandlerKind::Noop, handler);
    let result = exec
        .run_with_handlers(dot, "user-input", &serde_json::Map::new(), handlers)
        .await
        .expect("pipeline run");
    (result, state)
}

fn pos(order: &[String], id: &str) -> usize {
    order.iter().position(|n| n == id).expect("node ran")
}

const DIAMOND: &str = r#"digraph d {
    a [handler=codergen, tools=read_file, prompt="a"]
    b [handler=codergen, tools=read_file, prompt="b"]
    c [handler=codergen, tools=read_file, prompt="c"]
    d [handler=codergen, tools=read_file, prompt="d"]
    a -> b
    a -> c
    b -> d
    c -> d
}"#;

#[tokio::test]
async fn diamond_runs_both_branches_and_d_joins_both() {
    let (result, state) = run_with(DIAMOND, pass_all(), true).await;
    let st = state.lock().unwrap();
    assert!(result.success, "diamond should succeed: {}", result.output);
    for n in ["a", "b", "c", "d"] {
        assert!(
            st.calls.contains_key(n),
            "node {n} must run on the DAG path; order={:?}",
            st.order
        );
    }
    assert_eq!(
        st.preds_seen.get("d"),
        Some(&2),
        "d must JOIN both predecessors b and c; order={:?}",
        st.order
    );
    assert!(pos(&st.order, "a") < pos(&st.order, "b"));
    assert!(pos(&st.order, "a") < pos(&st.order, "c"));
    assert!(pos(&st.order, "b") < pos(&st.order, "d"));
    assert!(pos(&st.order, "c") < pos(&st.order, "d"));
}

#[tokio::test]
async fn single_path_half_runs_diamond_then_dag_fixes_it() {
    // The contrast that justifies the whole feature: on the single-path walk a
    // diamond half-runs — one branch never executes and d joins only one
    // predecessor. The DAG scheduler runs both and joins two.
    let (_r_legacy, legacy) = run_with(DIAMOND, pass_all(), false).await;
    {
        let legacy = legacy.lock().unwrap();
        assert_eq!(
            legacy.preds_seen.get("d"),
            Some(&1),
            "single-path walk feeds d only ONE predecessor (the latent bug)"
        );
        assert!(
            legacy.calls.len() < 4,
            "single-path walk skips a branch; ran {:?}",
            legacy.order
        );
    }

    let (_r_dag, dag) = run_with(DIAMOND, pass_all(), true).await;
    let dag = dag.lock().unwrap();
    assert_eq!(
        dag.preds_seen.get("d"),
        Some(&2),
        "DAG scheduler feeds d BOTH predecessors"
    );
}

#[tokio::test]
async fn conditional_routing_fires_only_matching_branch() {
    // a passes → the pass-edge to b fires; the fail-edge to c never fires, so
    // c is pruned and never runs.
    let dot = r#"digraph c {
        a [handler=codergen, tools=read_file, prompt="a"]
        b [handler=codergen, tools=read_file, prompt="b"]
        c [handler=codergen, tools=read_file, prompt="c"]
        a -> b [condition="outcome.status == \"pass\""]
        a -> c [condition="outcome.status == \"fail\""]
    }"#;
    let (result, state) = run_with(dot, pass_all(), true).await;
    let st = state.lock().unwrap();
    assert!(result.success);
    assert!(st.calls.contains_key("a") && st.calls.contains_key("b"));
    assert!(
        !st.calls.contains_key("c"),
        "the unmatched (fail) branch must be pruned; order={:?}",
        st.order
    );
}

#[tokio::test]
async fn fail_closed_prunes_unconditional_consumer_but_conditional_catches() {
    // b fails. The unconditional edge b→x must NOT fire (fail-closed → x
    // pruned). The conditional fail-edge b→handler runs (failure caught).
    let dot = r#"digraph f {
        a [handler=codergen, tools=read_file, prompt="a"]
        b [handler=codergen, tools=read_file, prompt="b"]
        x [handler=codergen, tools=read_file, prompt="x"]
        recover [handler=codergen, tools=read_file, prompt="recover"]
        a -> b
        b -> x
        b -> recover [condition="outcome.status == \"fail\""]
    }"#;
    let decide: Decider = Arc::new(|n, _c| {
        if n == "b" {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (_result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(st.calls.contains_key("a") && st.calls.contains_key("b"));
    assert!(
        !st.calls.contains_key("x"),
        "unconditional consumer of a Fail must be pruned (fail-closed); order={:?}",
        st.order
    );
    assert!(
        st.calls.contains_key("recover"),
        "conditional fail-edge must catch the failure; order={:?}",
        st.order
    );
}

#[tokio::test]
async fn bounded_retry_loop_terminates_on_pass() {
    // start → work → check; check loops back to work on fail (marked back-edge).
    // check fails twice then passes → work runs 3×, the loop settles, success.
    let dot = r#"digraph r {
        start [handler=codergen, tools=read_file, prompt="seed"]
        work [handler=codergen, tools=read_file, prompt="work"]
        check [handler=codergen, tools=read_file, prompt="check"]
        start -> work
        work -> check
        check -> work [label="back_edge", condition="outcome.status == \"fail\""]
    }"#;
    // `check` fails for the first few calls then passes. Note each failing
    // iteration costs TWO `check` calls — the dispatch plus the one M8.9
    // recovery re-attempt (legacy parity) — so the back-edge only fires once
    // the node is still failing after recovery.
    let decide: Decider = Arc::new(|n, c| {
        if n == "check" && c < 5 {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(
        result.success,
        "loop must settle to success: {}",
        result.output
    );
    let work_runs = st.calls.get("work").copied().unwrap_or(0);
    assert!(
        work_runs >= 2,
        "the back-edge must re-run work at least once after recovery; work ran {work_runs}, order={:?}",
        st.order
    );
}

#[tokio::test]
async fn linear_chain_runs_in_order() {
    let dot = r#"digraph l {
        a [handler=codergen, tools=read_file, prompt="a"]
        b [handler=codergen, tools=read_file, prompt="b"]
        c [handler=codergen, tools=read_file, prompt="c"]
        a -> b
        b -> c
    }"#;
    let (result, state) = run_with(dot, pass_all(), true).await;
    let st = state.lock().unwrap();
    assert!(result.success);
    assert_eq!(st.order, vec!["a", "b", "c"]);
    assert_eq!(st.preds_seen.get("c"), Some(&1));
}

// --- Regression guards for codex-review findings on the DAG scheduler. ---

#[tokio::test]
async fn terminal_fail_reports_failure_not_success() {
    // codex P1: a quiescent run whose terminal node settled Fail must report
    // success=false (the legacy walk derives success from the terminal outcome).
    let dot = r#"digraph t {
        a [handler=codergen, tools=read_file, prompt="a"]
        b [handler=codergen, tools=read_file, prompt="b"]
        a -> b
    }"#;
    let decide: Decider = Arc::new(|n, _c| {
        if n == "b" {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (result, _state) = run_with(dot, decide, true).await;
    assert!(
        !result.success,
        "a Fail terminal node must report success=false"
    );
}

#[tokio::test]
async fn retry_guard_hit_reports_failure() {
    // codex P1: a retry loop whose check NEVER passes must terminate via the
    // per-node run guard AND report failure (terminal check still Fail).
    let dot = r#"digraph r {
        start [handler=codergen, tools=read_file, prompt="seed"]
        work  [handler=codergen, tools=read_file, prompt="work"]
        check [handler=codergen, tools=read_file, prompt="check"]
        start -> work
        work -> check
        check -> work [label="back_edge", condition="outcome.status == \"fail\""]
    }"#;
    let decide: Decider = Arc::new(|n, _c| {
        if n == "check" {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(
        !result.success,
        "a loop that never passes must report failure, not empty success"
    );
    assert_eq!(
        st.calls.get("work"),
        Some(&11),
        "retry guard caps at MAX_NODE_RUNS=10 retries (+1 initial run); got {:?}",
        st.calls.get("work")
    );
}

#[tokio::test]
async fn retry_label_marker_recognized_as_back_edge() {
    // codex P1: validation permits a cycle marked label="retry" (not exactly
    // "back_edge"); the scheduler must treat it as a back-edge too, else it is a
    // forward cycle that deadlocks into an empty result.
    let dot = r#"digraph r {
        start [handler=codergen, tools=read_file, prompt="seed"]
        work  [handler=codergen, tools=read_file, prompt="work"]
        check [handler=codergen, tools=read_file, prompt="check"]
        start -> work
        work -> check
        check -> work [label="retry", condition="outcome.status == \"fail\""]
    }"#;
    // `c < 3` so the first iteration stays failing through its recovery
    // re-attempt, forcing the back-edge to fire at least once.
    let decide: Decider = Arc::new(|n, c| {
        if n == "check" && c < 3 {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(
        result.success,
        "retry-marked loop must run, not deadlock: {}",
        result.output
    );
    let work_runs = st.calls.get("work").copied().unwrap_or(0);
    assert!(
        work_runs >= 2,
        "retry-marked loop must re-run work via the back-edge; work ran {work_runs}"
    );
}

#[tokio::test]
async fn goal_gate_pass_ends_pipeline_early() {
    // codex P2: a passing goal_gate node ends the pipeline immediately — the
    // downstream consumer must not run.
    let dot = r#"digraph g {
        start [handler=codergen, tools=read_file, prompt="start"]
        goal  [handler=codergen, tools=read_file, goal_gate="true", prompt="goal"]
        after [handler=codergen, tools=read_file, prompt="after"]
        start -> goal
        goal -> after
    }"#;
    let (result, state) = run_with(dot, pass_all(), true).await;
    let st = state.lock().unwrap();
    assert!(result.success);
    assert!(st.calls.contains_key("goal"), "goal node must run");
    assert!(
        !st.calls.contains_key("after"),
        "goal_gate pass must end the pipeline before 'after'; ran {:?}",
        st.order
    );
}

#[tokio::test]
async fn back_edge_carries_feedback_to_retried_node() {
    // codex P1: when the back-edge retries `work`, the re-run must SEE `check`'s
    // critique (its content) in its input, not just the original seed — else a
    // feedback-driven repair loop never converges. `check`'s output is
    // "check#N"; assert the retried `work` input carries it.
    let dot = r#"digraph r {
        start [handler=codergen, tools=read_file, prompt="seed"]
        work  [handler=codergen, tools=read_file, prompt="work"]
        check [handler=codergen, tools=read_file, prompt="check"]
        start -> work
        work -> check
        check -> work [label="back_edge", condition="outcome.status == \"fail\""]
    }"#;
    let decide: Decider = Arc::new(|n, c| {
        if n == "check" && c < 3 {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(result.success);
    let work_input = st.inputs.get("work").cloned().unwrap_or_default();
    assert!(
        work_input.contains("check"),
        "the retried work node must see check's feedback in its input; got {work_input:?}"
    );
}

#[tokio::test]
async fn unconditional_fallback_suppressed_when_conditional_matches() {
    // codex P2: matching conditions take precedence over an unconditional
    // fallback (legacy select_next_edge order). Only `matched` runs; `default`
    // must NOT also fire on a passing source.
    let dot = r#"digraph p {
        a       [handler=codergen, tools=read_file, prompt="a"]
        matched [handler=codergen, tools=read_file, prompt="matched"]
        default [handler=codergen, tools=read_file, prompt="default"]
        a -> matched [condition="outcome.status == \"pass\""]
        a -> default
    }"#;
    let (result, state) = run_with(dot, pass_all(), true).await;
    let st = state.lock().unwrap();
    assert!(result.success);
    assert!(st.calls.contains_key("matched"), "matched branch must run");
    assert!(
        !st.calls.contains_key("default"),
        "unconditional default must be suppressed when a condition matched; ran {:?}",
        st.order
    );
}

#[tokio::test]
async fn back_edge_retries_before_failure_consumer_runs() {
    // codex P2: `check` has BOTH a retry back-edge to `work` AND a forward
    // fail-edge to `report`. A TRANSIENT failed check must retry the loop, not
    // run `report` — the failure consumer should only run once the failure is
    // real. Here the loop settles to success, so `report` must never run.
    let dot = r#"digraph r {
        start  [handler=codergen, tools=read_file, prompt="seed"]
        work   [handler=codergen, tools=read_file, prompt="work"]
        check  [handler=codergen, tools=read_file, prompt="check"]
        done   [handler=codergen, tools=read_file, prompt="done"]
        report [handler=codergen, tools=read_file, prompt="report"]
        start -> work
        work -> check
        check -> work [label="back_edge", condition="outcome.status == \"fail\""]
        check -> done [condition="outcome.status == \"pass\""]
        check -> report [condition="outcome.status == \"fail\""]
    }"#;
    let decide: Decider = Arc::new(|n, c| {
        if n == "check" && c < 3 {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(
        result.success,
        "loop should settle to success: {}",
        result.output
    );
    assert!(
        st.calls.contains_key("done"),
        "the pass path must run on settle"
    );
    assert!(
        !st.calls.contains_key("report"),
        "failure consumer must NOT run on a transient (retried) failure; ran {:?}",
        st.order
    );
}

#[tokio::test]
async fn join_pruned_when_a_required_predecessor_fails() {
    // codex P1: a join must NOT run with partial input. In a diamond, b passes
    // but c fails → c->d is fail-closed; d must be pruned (not run with only b's
    // output), and the run must report failure.
    let dot = r#"digraph j {
        a [handler=codergen, tools=read_file, prompt="a"]
        b [handler=codergen, tools=read_file, prompt="b"]
        c [handler=codergen, tools=read_file, prompt="c"]
        d [handler=codergen, tools=read_file, prompt="d"]
        a -> b
        a -> c
        b -> d
        c -> d
    }"#;
    let decide: Decider = Arc::new(|n, _c| {
        if n == "c" {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(st.calls.contains_key("b") && st.calls.contains_key("c"));
    assert!(
        !st.calls.contains_key("d"),
        "join must not run with a missing (failed) predecessor; order={:?}",
        st.order
    );
    assert!(
        !result.success,
        "a failed required join input must make the run fail"
    );
}

#[tokio::test]
async fn failure_prune_propagates_through_intermediate_node() {
    // codex P1 (multi-hop): a->{b,c}, c->e, {b,e}->d. c fails → e is
    // failure-pruned, and that must PROPAGATE so d (joining b and e) is also
    // pruned, never run on partial input.
    let dot = r#"digraph m {
        a [handler=codergen, tools=read_file, prompt="a"]
        b [handler=codergen, tools=read_file, prompt="b"]
        c [handler=codergen, tools=read_file, prompt="c"]
        e [handler=codergen, tools=read_file, prompt="e"]
        d [handler=codergen, tools=read_file, prompt="d"]
        a -> b
        a -> c
        c -> e
        b -> d
        e -> d
    }"#;
    let decide: Decider = Arc::new(|n, _c| {
        if n == "c" {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(st.calls.contains_key("b"), "b runs");
    assert!(
        !st.calls.contains_key("e"),
        "e must be pruned (its required input c failed); order={:?}",
        st.order
    );
    assert!(
        !st.calls.contains_key("d"),
        "d must be pruned by the PROPAGATED failure, not run on partial input; order={:?}",
        st.order
    );
    assert!(!result.success, "a propagated failure must fail the run");
}

#[tokio::test]
async fn all_conditional_router_falls_back_to_lowest_target() {
    // codex P2: a passing node whose outgoing edges are ALL conditional and
    // none match routes to the lowest-target edge (legacy select_next_edge
    // Step-6 fallback), rather than dead-ending. bbb < zzz, so bbb runs.
    let dot = r#"digraph f {
        a   [handler=codergen, tools=read_file, prompt="a"]
        zzz [handler=codergen, tools=read_file, prompt="zzz"]
        bbb [handler=codergen, tools=read_file, prompt="bbb"]
        a -> zzz [condition="outcome.status == \"fail\""]
        a -> bbb [condition="outcome.status == \"fail\""]
    }"#;
    let (result, state) = run_with(dot, pass_all(), true).await;
    let st = state.lock().unwrap();
    assert!(result.success);
    assert!(
        st.calls.contains_key("bbb"),
        "lowest-target fallback (bbb < zzz) must run; order={:?}",
        st.order
    );
    assert!(
        !st.calls.contains_key("zzz"),
        "only the lowest-target fallback fires; order={:?}",
        st.order
    );
}

#[tokio::test]
async fn recovery_branch_rejoins_via_conditional_pass_edge() {
    // Strict fail-closed: a recovery-rejoin is expressed with a CONDITIONAL
    // success edge `b -> d [pass]` (a conditional miss on failure, not a hard
    // dependency) plus a recovery path `b -> recover [fail]`, `recover -> d`.
    // When b fails, b->d[pass] simply doesn't fire and d rejoins via recover.
    let dot = r#"digraph r {
        a       [handler=codergen, tools=read_file, prompt="a"]
        b       [handler=codergen, tools=read_file, prompt="b"]
        recover [handler=codergen, tools=read_file, prompt="recover"]
        d       [handler=codergen, tools=read_file, prompt="d"]
        a -> b
        b -> d [condition="outcome.status == \"pass\""]
        b -> recover [condition="outcome.status == \"fail\""]
        recover -> d
    }"#;
    let decide: Decider = Arc::new(|n, _c| {
        if n == "b" {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(
        st.calls.contains_key("recover"),
        "recover must run on b's failure"
    );
    assert!(
        st.calls.contains_key("d"),
        "d must rejoin after recovery, not be pruned; order={:?}",
        st.order
    );
    assert!(
        result.success,
        "the recovered flow should succeed: {}",
        result.output
    );
}

#[tokio::test]
async fn unrelated_failure_handler_does_not_catch_join() {
    // codex P2: c->recover[fail] is a failure handler that does NOT reach d, so
    // c's failure is NOT caught for the d join. d must still be pruned (missing
    // c's input), not run on partial input from only b.
    let dot = r#"digraph u {
        a       [handler=codergen, tools=read_file, prompt="a"]
        b       [handler=codergen, tools=read_file, prompt="b"]
        c       [handler=codergen, tools=read_file, prompt="c"]
        d       [handler=codergen, tools=read_file, prompt="d"]
        recover [handler=codergen, tools=read_file, prompt="recover"]
        a -> b
        a -> c
        b -> d
        c -> d
        c -> recover [condition="outcome.status == \"fail\""]
    }"#;
    let decide: Decider = Arc::new(|n, _c| {
        if n == "c" {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (_result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(
        st.calls.contains_key("recover"),
        "recover runs on c's failure"
    );
    assert!(
        !st.calls.contains_key("d"),
        "d must be pruned: recover does not feed d, so c's input stays missing; order={:?}",
        st.order
    );
}

#[tokio::test]
async fn retry_loop_back_to_start_is_bounded() {
    // A retry edge back to the START node is a valid DAG loop (start is the
    // root). check always fails → start re-runs, bounded by MAX_NODE_RUNS, then
    // the run reports failure (it must NOT silently fall back to legacy).
    let dot = r#"digraph s {
        start [handler=codergen, tools=read_file, prompt="start"]
        check [handler=codergen, tools=read_file, prompt="check"]
        start -> check
        check -> start [label="back_edge", condition="outcome.status == \"fail\""]
    }"#;
    let decide: Decider = Arc::new(|n, _c| {
        if n == "check" {
            OutcomeStatus::Fail
        } else {
            OutcomeStatus::Pass
        }
    });
    let (result, state) = run_with(dot, decide, true).await;
    let st = state.lock().unwrap();
    assert!(!result.success, "a never-passing loop must report failure");
    assert_eq!(
        st.calls.get("start"),
        Some(&11),
        "start re-runs bounded by MAX_NODE_RUNS=10 retries (+1 initial); got {:?}",
        st.calls.get("start")
    );
}

#[tokio::test]
async fn back_edge_firing_before_target_runs_is_bounded() {
    // codex P1: a->b[back_edge, pass] fires when `a` passes, but `b` is pruned
    // (its input x is a not-taken branch) and never runs. The retry guard counts
    // RETRIES (not target runs), so this terminates instead of spinning forever.
    // The test merely RETURNING proves the loop is bounded.
    let dot = r#"digraph s {
        start [handler=codergen, tools=read_file, prompt="start"]
        a     [handler=codergen, tools=read_file, prompt="a"]
        x     [handler=codergen, tools=read_file, prompt="x"]
        b     [handler=codergen, tools=read_file, prompt="b"]
        start -> a
        start -> x [condition="outcome.status == \"fail\""]
        x -> b
        b -> a
        a -> b [label="back_edge", condition="outcome.status == \"pass\""]
    }"#;
    let (_result, state) = run_with(dot, pass_all(), true).await;
    let st = state.lock().unwrap();
    assert!(st.calls.contains_key("a"), "a runs");
    assert!(
        st.calls.get("a").copied().unwrap_or(0) <= 12,
        "a must be bounded by the retry guard, not spin forever; got {:?}",
        st.calls.get("a")
    );
}
