//! Acceptance tests for RP04 — mission checkpoint persistence & resume.
//!
//! After a node declaring `MissionCheckpoint`s completes, the executor
//! persists one `PersistedCheckpoint` per declaration via the configured
//! `CheckpointStore`. On a subsequent run, the executor skips every node
//! whose id appears in any persisted checkpoint.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_core::TokenUsage;
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage as LlmTokenUsage};
use octos_memory::EpisodeStore;
use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor};
use octos_pipeline::handler::HandlerContext;
use octos_pipeline::{
    CheckpointStore, FileSystemCheckpointStore, Handler, HandlerKind, HandlerRegistry,
    MissionCheckpoint, NodeOutcome, NoopHandler, OutcomeStatus, PersistedCheckpoint, PipelineNode,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct CountingHandler {
    invocations: Arc<AtomicUsize>,
    seen_ids: Arc<std::sync::Mutex<Vec<String>>>,
}

impl CountingHandler {
    fn new() -> (
        Arc<Self>,
        Arc<AtomicUsize>,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let counter = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler = Arc::new(Self {
            invocations: counter.clone(),
            seen_ids: seen.clone(),
        });
        (handler, counter, seen)
    }
}

#[async_trait]
impl Handler for CountingHandler {
    async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
        self.invocations.fetch_add(1, Ordering::Relaxed);
        self.seen_ids.lock().expect("lock").push(node.id.clone());
        // Yield to ensure we don't starve any concurrent work.
        tokio::time::sleep(Duration::from_millis(5)).await;
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: OutcomeStatus::Pass,
            content: format!("out-{}", node.id),
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
    store: Option<Arc<dyn CheckpointStore>>,
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
        checkpoint_store: store,
        hook_executor: None,
        workspace_context: octos_pipeline::context::PipelineContext::default(),
        host_context: octos_pipeline::host_context::PipelineHostContext::default(),
    }
}

fn handlers_with(sleep_handler: Arc<dyn Handler>) -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    registry.register(HandlerKind::Noop, sleep_handler);
    registry.register(HandlerKind::Codergen, Arc::new(NoopHandler));
    registry.register(HandlerKind::Gate, Arc::new(NoopHandler));
    registry.register(HandlerKind::Shell, Arc::new(NoopHandler));
    registry.register(HandlerKind::Parallel, Arc::new(NoopHandler));
    registry.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));
    registry
}

const THREE_NODE_DOT: &str = r#"
    digraph mission {
        start [handler="noop"]
        mid   [handler="noop", checkpoint="post_mid"]
        end   [handler="noop"]
        start -> mid
        mid -> end
    }
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_persist_checkpoint_after_node_completion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = temp_episode_store(dir.path()).await;
    let store: Arc<dyn CheckpointStore> = Arc::new(FileSystemCheckpointStore::new(dir.path()));

    let (handler, _counter, _seen) = CountingHandler::new();
    let executor = PipelineExecutor::new(base_config(
        dir.path().to_path_buf(),
        memory,
        Some(store.clone()),
    ));

    let result = executor
        .run_with_handlers(
            THREE_NODE_DOT,
            "hello",
            &serde_json::Map::new(),
            handlers_with(handler),
        )
        .await
        .expect("run must succeed");
    assert!(result.success);

    let persisted = store.list().expect("list");
    assert_eq!(persisted.len(), 1, "one checkpoint from `mid` node");
    assert_eq!(persisted[0].node_id, "mid");
    assert_eq!(persisted[0].name, "post_mid");
    assert!(persisted[0].resumable);
}

#[tokio::test]
async fn should_skip_completed_nodes_on_resume_from_checkpoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = temp_episode_store(dir.path()).await;
    let store: Arc<dyn CheckpointStore> = Arc::new(FileSystemCheckpointStore::new(dir.path()));

    // --- First run: complete once, leaving a checkpoint from `mid`. ---
    let (handler1, counter1, seen1) = CountingHandler::new();
    {
        let executor = PipelineExecutor::new(base_config(
            dir.path().to_path_buf(),
            memory.clone(),
            Some(store.clone()),
        ));
        executor
            .run_with_handlers(
                THREE_NODE_DOT,
                "hello",
                &serde_json::Map::new(),
                handlers_with(handler1),
            )
            .await
            .expect("first run");
    }

    // The first run executed all three nodes.
    assert_eq!(counter1.load(Ordering::Relaxed), 3);
    assert_eq!(
        seen1.lock().expect("lock").as_slice(),
        &["start".to_string(), "mid".to_string(), "end".to_string()]
    );

    // --- Second run: executor must observe the persisted checkpoint and
    // skip nodes `start` and `mid`, only running `end`. ---
    let (handler2, counter2, seen2) = CountingHandler::new();
    let executor = PipelineExecutor::new(base_config(
        dir.path().to_path_buf(),
        memory.clone(),
        Some(store.clone()),
    ));

    // Write a richer checkpoint that also records "start" so the skip set
    // covers everything up to and including "mid".
    let decl = MissionCheckpoint {
        name: "post_start".into(),
        resumable: true,
    };
    let extra = PersistedCheckpoint::from_declaration("mission", "start", &decl, 99);
    store.persist(&extra).expect("persist extra");

    executor
        .run_with_handlers(
            THREE_NODE_DOT,
            "hello",
            &serde_json::Map::new(),
            handlers_with(handler2),
        )
        .await
        .expect("second run");

    // Only `end` should have actually been invoked; `start` and `mid` were
    // both in the skip set.
    assert_eq!(
        counter2.load(Ordering::Relaxed),
        1,
        "second run skips start & mid, runs end only"
    );
    assert_eq!(seen2.lock().expect("lock").as_slice(), &["end".to_string()]);
}

#[test]
fn should_parse_inspection_mission_fixture() {
    let dot = include_str!("fixtures/inspection_mission.dot");
    let graph = octos_pipeline::parse_dot(dot).expect("fixture must parse");
    // Every node with a deadline must parse a valid action.
    for node in graph.nodes.values() {
        if node.deadline_secs.is_some() {
            assert!(
                node.deadline_action.is_some(),
                "node {} has deadline but no action",
                node.id
            );
        }
    }
    // Checkpoint names are taken from the `checkpoint` attribute.
    assert!(
        graph
            .nodes
            .values()
            .any(|n| n.checkpoints.iter().any(|c| c.name == "post_navigate")),
        "fixture must declare a 'post_navigate' checkpoint"
    );
}

#[test]
fn should_write_checkpoint_atomically_via_temp_rename() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FileSystemCheckpointStore::new(dir.path());

    let decl = MissionCheckpoint {
        name: "atomic".into(),
        resumable: true,
    };
    let cp = PersistedCheckpoint::from_declaration("g", "n", &decl, 0);
    store.persist(&cp).expect("persist");

    // No `.tmp` sibling should remain after a successful rename.
    let tmp_path = dir.path().join("mission_checkpoints.json.tmp");
    assert!(
        !tmp_path.exists(),
        "atomic persist must leave no temp file behind"
    );
    assert!(dir.path().join("mission_checkpoints.json").exists());
}
