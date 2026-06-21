//! Regression tests for the two code-review findings on the fan-out hang fix
//! (PR #1427):
//!
//!   1. A fan-out worker that exceeds its `deadline_secs` must route through the
//!      SAME `deadline_action` machinery (`skip` / `retry` / `escalate`) the
//!      single-node `dispatch_node` path uses — not be unconditionally turned
//!      into an `Err` that aborts the fan-out.
//!
//!   2. The new "all workers failed → terminate the pipeline" branch must NOT
//!      fire when the fan-out node sets `continue_on_error = true`. In that case
//!      the pipeline intentionally tolerates a fully-failed fan-out and lets the
//!      normal convergence / error routing proceed.
//!
//! These drive the real static `parallel` fan-out path with injected handlers
//! and assert behavior in bounded wall-clock time. The companion
//! `fanout_failed_task_no_hang.rs` tests pin that none of this reintroduces the
//! production wedge (a never-resolving worker must still terminate).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// A handler that, for any node whose id is in `hang_nodes`, hangs FOREVER (the
/// production never-resolving worker). For any node whose id is in
/// `error_nodes`, returns an `Error` outcome immediately. Every other node
/// passes. Each `execute` call increments `calls` so a test can assert retry
/// attempts actually happened.
struct ScriptedHandler {
    hang_nodes: Vec<String>,
    error_nodes: Vec<String>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedHandler {
    fn new(hang_nodes: Vec<String>, error_nodes: Vec<String>) -> (Arc<Self>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(Self {
            hang_nodes,
            error_nodes,
            calls: calls.clone(),
        });
        (handler, calls)
    }
}

#[async_trait]
impl Handler for ScriptedHandler {
    async fn execute(
        &self,
        node: &PipelineNode,
        _ctx: &HandlerContext,
    ) -> eyre::Result<NodeOutcome> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.hang_nodes.iter().any(|n| n == &node.id) {
            std::future::pending::<()>().await;
            unreachable!("hanging worker future must never resolve");
        }
        let status = if self.error_nodes.iter().any(|n| n == &node.id) {
            OutcomeStatus::Error
        } else {
            OutcomeStatus::Pass
        };
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status,
            content: format!("{} {:?}", node.id, status),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        })
    }
}

async fn make_executor(dir: &TempDir) -> PipelineExecutor {
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
    PipelineExecutor::new(config)
}

fn handlers_for(handler: Arc<ScriptedHandler>) -> HandlerRegistry {
    let mut handlers = HandlerRegistry::new();
    handlers.register(HandlerKind::Codergen, handler.clone());
    handlers.register(HandlerKind::Noop, handler.clone());
    handlers.register(HandlerKind::Parallel, handler.clone());
    handlers.register(HandlerKind::Gate, handler);
    handlers
}

/// (a) Fan-out workers that time out with `deadline_action=skip` must be
/// SKIPPED (not turned into hard Errors that abort the fan-out). With EVERY
/// branch skipped, the old code (unconditional `Err` → all-failed → abort)
/// terminates the pipeline; the fix routes the expiry through the
/// `deadline_action` machinery so the branches are `Skipped` (carry no error),
/// the fan-out converges, `merge` runs and the pipeline succeeds.
#[tokio::test]
async fn fanout_worker_deadline_skip_continues_not_aborts() {
    let dir = TempDir::new().unwrap();
    let exec = make_executor(&dir).await;

    // BOTH branches hang forever but declare deadline_action=skip. Each
    // deadline expiry must produce a SKIPPED outcome (neither pass nor hard
    // error), so the fan-out is NOT all-failed and convergence proceeds.
    let dot = r#"digraph fanout {
        fan  [handler="parallel", converge="merge"]
        s1 [handler="codergen", tools=read_file, prompt="s1", deadline_secs="1", deadline_action="skip"]
        s2 [handler="codergen", tools=read_file, prompt="s2", deadline_secs="1", deadline_action="skip"]
        merge [handler="codergen", tools=read_file, prompt="merge"]
        fan -> s1
        fan -> s2
        s1 -> merge
        s2 -> merge
    }"#;

    let (handler, _calls) = ScriptedHandler::new(vec!["s1".into(), "s2".into()], vec![]);

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
    .expect("pipeline wedged: deadline_action=skip worker did not get cut off")
    .expect("pipeline run returned an error result");

    // No branch hard-errored (both skipped), so the all-failed termination must
    // NOT fire; the converge node ran and passed.
    assert!(
        !result.output.contains("workers failed"),
        "deadline_action=skip on every branch must NOT trip the all-failed \
         abort; got: {}",
        result.output
    );
    assert!(
        result.success,
        "deadline_action=skip must let the fan-out continue (skipped != error), \
         got success={} output={}",
        result.success, result.output
    );
}

/// (b) A fan-out worker that times out with `deadline_action=retry:N` must
/// actually re-attempt — `execute` is invoked more than once for that node.
#[tokio::test]
async fn fanout_worker_deadline_retry_attempts_retries() {
    let dir = TempDir::new().unwrap();
    let exec = make_executor(&dir).await;

    // `flaky` hangs forever with a short deadline and retry:3. Each attempt is
    // bounded by the deadline (1s), so all 3 attempts time out, but the handler
    // must have been ENTERED 3 times for `flaky` — proving the retry action ran
    // rather than a single unconditional Err.
    let dot = r#"digraph fanout {
        fan  [handler="parallel", converge="merge"]
        good [handler="codergen", tools=read_file, prompt="good"]
        flaky [handler="codergen", tools=read_file, prompt="flaky", deadline_secs="1", deadline_action="retry:3"]
        merge [handler="codergen", tools=read_file, prompt="merge"]
        fan -> good
        fan -> flaky
        good -> merge
        flaky -> merge
    }"#;

    let (handler, calls) = ScriptedHandler::new(vec!["flaky".into()], vec![]);

    let _ = tokio::time::timeout(
        Duration::from_secs(30),
        exec.run_with_handlers(
            dot,
            "user-input",
            &serde_json::Map::new(),
            handlers_for(handler),
        ),
    )
    .await
    .expect("pipeline wedged: retry worker did not terminate within its bounded attempts")
    .expect("pipeline run returned an error result");

    // 1 entry for `good` + 3 attempts for `flaky` = at least 4 handler entries.
    // The load-bearing assertion is that `flaky` was entered MORE THAN ONCE.
    let total = calls.load(Ordering::Relaxed);
    assert!(
        total >= 4,
        "deadline_action=retry:3 must re-attempt the hung worker (>1 entry); \
         saw {total} total handler entries (expected >=4: 1 good + 3 flaky)"
    );
}

/// (c) An all-failed fan-out with `continue_on_error=true` must NOT abort the
/// pipeline at the fan-out. Convergence runs on the (failed) merged content and
/// the pipeline proceeds — here `merge` passes, so the run succeeds.
#[tokio::test]
async fn fanout_all_failed_with_continue_on_error_does_not_abort() {
    let dir = TempDir::new().unwrap();
    let exec = make_executor(&dir).await;

    // Both workers return Error → all-failed fan-out. But the fan-out node opts
    // in with continue_on_error=true, so the pipeline must NOT terminate at the
    // fan-out; it jumps to `merge`, which passes.
    let dot = r#"digraph fanout {
        fan  [handler="parallel", converge="merge", continue_on_error="true"]
        e1 [handler="codergen", tools=read_file, prompt="e1"]
        e2 [handler="codergen", tools=read_file, prompt="e2"]
        merge [handler="codergen", tools=read_file, prompt="merge"]
        fan -> e1
        fan -> e2
        e1 -> merge
        e2 -> merge
    }"#;

    let (handler, _calls) = ScriptedHandler::new(vec![], vec!["e1".into(), "e2".into()]);

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
    .expect("pipeline wedged")
    .expect("pipeline run returned an error result");

    // continue_on_error short-circuits the all-failed abort; the converge node
    // ran and passed, so the overall run succeeds. The KEY assertion is that
    // the output is NOT the all-workers-failed abort message.
    assert!(
        !result.output.contains("all 2 workers failed"),
        "continue_on_error=true must NOT abort an all-failed fan-out; \
         got abort output: {}",
        result.output
    );
    assert!(
        result.success,
        "with continue_on_error=true the fan-out tolerates total failure and \
         convergence proceeds (merge passed), got success={} output={}",
        result.success, result.output
    );
}

/// (d) The default (continue_on_error=false) all-failed fan-out STILL
/// terminates the pipeline — the production-hang fix is preserved. This is the
/// same shape as (c) minus the `continue_on_error` attribute.
#[tokio::test]
async fn fanout_all_failed_default_still_terminates() {
    let dir = TempDir::new().unwrap();
    let exec = make_executor(&dir).await;

    let dot = r#"digraph fanout {
        fan  [handler="parallel", converge="merge"]
        e1 [handler="codergen", tools=read_file, prompt="e1"]
        e2 [handler="codergen", tools=read_file, prompt="e2"]
        merge [handler="codergen", tools=read_file, prompt="merge"]
        fan -> e1
        fan -> e2
        e1 -> merge
        e2 -> merge
    }"#;

    let (handler, _calls) = ScriptedHandler::new(vec![], vec!["e1".into(), "e2".into()]);

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
    .expect("pipeline wedged")
    .expect("pipeline run returned an error result");

    assert!(
        !result.success,
        "default (continue_on_error=false) all-failed fan-out must FAIL the \
         pipeline, got success={}",
        result.success
    );
    assert!(
        result.output.contains("all 2 workers failed"),
        "expected the all-workers-failed abort message, got: {}",
        result.output
    );
}
