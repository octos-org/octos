//! Acceptance tests for RP04 — per-node deadline enforcement.
//!
//! The pipeline executor wraps each node's execution in
//! `tokio::time::timeout(deadline_secs)` when the node declares a deadline,
//! and dispatches on `deadline_action`. Each variant produces a distinct,
//! observable effect:
//!
//! * `Abort`    -> pipeline error propagates, counter "abort" increments
//! * `Skip`     -> node marked `OutcomeStatus::Skipped`, pipeline continues
//! * `Retry`    -> node re-executed up to `max_attempts` times before aborting
//! * `Escalate` -> fires `HookEvent::OnSpawnFailure` before aborting
//!
//! The tests drive a real `PipelineExecutor` with a custom `Handler` that
//! sleeps for a known duration, injected via `run_with_handlers`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use octos_agent::hooks::{HookConfig, HookEvent, HookExecutor};
use octos_core::TokenUsage;
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage as LlmTokenUsage};
use octos_memory::EpisodeStore;
use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor};
use octos_pipeline::handler::HandlerContext;
use octos_pipeline::{
    Handler, HandlerKind, HandlerRegistry, NodeOutcome, NoopHandler, OutcomeStatus, PipelineNode,
    deadline_exceeded_count,
};

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

/// A handler that sleeps before returning success. Counts invocations so
/// tests can assert retry behavior.
struct SleepHandler {
    duration: Duration,
    invocations: Arc<AtomicUsize>,
}

impl SleepHandler {
    fn new(duration: Duration) -> (Arc<Self>, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(Self {
            duration,
            invocations: counter.clone(),
        });
        (handler, counter)
    }
}

#[async_trait]
impl Handler for SleepHandler {
    async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
        self.invocations.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(self.duration).await;
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: OutcomeStatus::Pass,
            content: String::from("done"),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        })
    }
}

struct MockProvider;

#[async_trait]
impl LlmProvider for MockProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        Ok(ChatResponse {
            content: Some("ok".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: LlmTokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

async fn temp_episode_store(dir: &std::path::Path) -> Arc<EpisodeStore> {
    Arc::new(EpisodeStore::open(dir).await.expect("open episode store"))
}

fn base_config(
    working_dir: PathBuf,
    memory: Arc<EpisodeStore>,
    hook_executor: Option<Arc<HookExecutor>>,
) -> ExecutorConfig {
    ExecutorConfig {
        default_provider: Arc::new(MockProvider) as Arc<dyn LlmProvider>,
        provider_router: None,
        memory,
        working_dir,
        provider_policy: None,
        plugin_dirs: vec![],
        status_bridge: None,
        shutdown: Arc::new(AtomicBool::new(false)),
        max_parallel_workers: 1,
        max_pipeline_fanout_total: None,
        checkpoint_store: None,
        hook_executor,
        workspace_context: octos_pipeline::context::PipelineContext::default(),
        host_context: octos_pipeline::host_context::PipelineHostContext::default(),
    }
}

/// Build a `HandlerRegistry` with the `SleepHandler` bound to
/// `HandlerKind::Shell`. All other slots use the cheap built-in `NoopHandler`
/// so only `handler="shell"` nodes stall in tests.
fn handlers_with_sleep(sleep_handler: Arc<dyn Handler>) -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    registry.register(HandlerKind::Shell, sleep_handler);
    registry.register(HandlerKind::Noop, Arc::new(NoopHandler));
    registry.register(HandlerKind::Codergen, Arc::new(NoopHandler));
    registry.register(HandlerKind::Gate, Arc::new(NoopHandler));
    registry.register(HandlerKind::Parallel, Arc::new(NoopHandler));
    registry.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));
    registry
}

fn single_slow_node_dot(action: &str) -> String {
    // `start` uses a fast built-in handler (Codergen -> NoopHandler in the
    // test registry); `slow` uses the custom SleepHandler bound to
    // HandlerKind::Shell. This separation lets us assert deadline bounds
    // precisely — only the `slow` node stalls.
    format!(
        r#"
        digraph t {{
            start [handler="codergen"]
            slow [handler="shell", deadline_secs="1", deadline_action="{action}"]
            start -> slow
        }}
        "#
    )
}

// ---------------------------------------------------------------------------
// Acceptance tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_abort_node_when_deadline_exceeded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = temp_episode_store(dir.path()).await;
    let (handler, counter) = SleepHandler::new(Duration::from_secs(5));

    let executor = PipelineExecutor::new(base_config(dir.path().to_path_buf(), memory, None));
    let handlers = handlers_with_sleep(handler);

    let before = deadline_exceeded_count("abort");
    let start = Instant::now();
    let res = executor
        .run_with_handlers(
            &single_slow_node_dot("abort"),
            "input",
            &serde_json::Map::new(),
            handlers,
        )
        .await;
    let elapsed = start.elapsed();

    assert!(res.is_err(), "abort variant must return an error");
    assert!(
        elapsed < Duration::from_millis(1_500),
        "abort must fire within 1.5s (got {:?})",
        elapsed
    );
    assert!(
        counter.load(Ordering::Relaxed) >= 1,
        "the slow handler was invoked"
    );
    let after = deadline_exceeded_count("abort");
    assert_eq!(
        after,
        before + 1,
        "abort counter must increment exactly once"
    );
}

#[tokio::test]
async fn should_skip_node_when_deadline_action_is_skip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = temp_episode_store(dir.path()).await;
    let (handler, counter) = SleepHandler::new(Duration::from_secs(5));

    let executor = PipelineExecutor::new(base_config(dir.path().to_path_buf(), memory, None));
    let handlers = handlers_with_sleep(handler);

    let before = deadline_exceeded_count("skip");
    let start = Instant::now();
    let result = executor
        .run_with_handlers(
            &single_slow_node_dot("skip"),
            "input",
            &serde_json::Map::new(),
            handlers,
        )
        .await
        .expect("skip must return Ok");
    let elapsed = start.elapsed();

    assert!(
        !result.success,
        "a skipped node does not count as a successful run"
    );
    assert!(
        elapsed < Duration::from_millis(1_500),
        "skip must fire within 1.5s (got {:?})",
        elapsed
    );
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    assert_eq!(
        deadline_exceeded_count("skip"),
        before + 1,
        "skip counter must increment"
    );
}

#[tokio::test]
async fn should_retry_node_up_to_max_attempts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = temp_episode_store(dir.path()).await;
    let (handler, counter) = SleepHandler::new(Duration::from_secs(5));

    let executor = PipelineExecutor::new(base_config(dir.path().to_path_buf(), memory, None));
    let handlers = handlers_with_sleep(handler);

    let before = deadline_exceeded_count("retry");
    let start = Instant::now();
    let res = executor
        .run_with_handlers(
            &single_slow_node_dot("retry:3"),
            "input",
            &serde_json::Map::new(),
            handlers,
        )
        .await;
    let elapsed = start.elapsed();

    assert!(
        res.is_err(),
        "after exhausting retries the pipeline must fail"
    );
    assert_eq!(
        counter.load(Ordering::Relaxed),
        3,
        "handler invoked exactly max_attempts times"
    );
    // 3 × 1s ≈ 3s; allow generous slack for scheduling.
    assert!(
        elapsed < Duration::from_millis(4_500),
        "retry must bound total time (got {:?})",
        elapsed
    );
    assert_eq!(
        deadline_exceeded_count("retry"),
        before + 3,
        "retry counter must increment per attempt"
    );
}

#[tokio::test]
async fn should_fire_spawn_failure_hook_when_deadline_action_is_escalate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = temp_episode_store(dir.path()).await;
    let (handler, counter) = SleepHandler::new(Duration::from_secs(5));

    // Hook writes a marker file; we verify that the file exists after the
    // executor runs, proving the hook fired.
    let marker = dir.path().join("hook_fired");
    let marker_str = marker.to_string_lossy().to_string();

    #[cfg(unix)]
    let cmd = vec!["sh".into(), "-c".into(), format!("touch {}", marker_str)];
    #[cfg(not(unix))]
    let cmd = vec![
        "cmd".into(),
        "/C".into(),
        format!("type nul > {}", marker_str),
    ];

    let hook_cfg = HookConfig {
        event: HookEvent::OnSpawnFailure,
        command: cmd,
        timeout_ms: 3000,
        tool_filter: vec![],
    };
    let hook = Arc::new(HookExecutor::new(vec![hook_cfg]));

    let executor = PipelineExecutor::new(base_config(
        dir.path().to_path_buf(),
        memory,
        Some(hook.clone()),
    ));
    let handlers = handlers_with_sleep(handler);

    let before = deadline_exceeded_count("escalate");
    let start = Instant::now();
    let res = executor
        .run_with_handlers(
            &single_slow_node_dot("escalate"),
            "input",
            &serde_json::Map::new(),
            handlers,
        )
        .await;
    let elapsed = start.elapsed();

    assert!(res.is_err(), "escalate variant must still error");
    assert!(
        elapsed < Duration::from_millis(3_500),
        "escalate must fire quickly (got {:?})",
        elapsed
    );
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    assert_eq!(
        deadline_exceeded_count("escalate"),
        before + 1,
        "escalate counter must increment"
    );
    assert!(
        marker.exists(),
        "OnSpawnFailure hook must have fired, creating {}",
        marker.display()
    );
}
