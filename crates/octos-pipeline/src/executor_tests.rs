use super::*;
use crate::graph::{NodeOutcome, OutcomeStatus, PipelineNode};
use crate::guard::{TimeoutGuard, TokenBudgetGuard};
use crate::handler::Handler;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

#[test]
fn sanitize_label_for_filename_yields_a_deterministic_fs_safe_token() {
    // deep_research workers write `findings-{label}.md`; the substituted token
    // must be stable + filesystem-safe so the analyze node reads it back by name.
    assert_eq!(
        sanitize_label_for_filename("Official Docs"),
        "official_docs"
    );
    assert_eq!(
        sanitize_label_for_filename("Alternatives / Comparisons"),
        "alternatives_comparisons"
    );
    assert_eq!(
        sanitize_label_for_filename("  Recent Trends!!!  "),
        "recent_trends"
    );
    assert_eq!(sanitize_label_for_filename("Task 3"), "task_3");
    // No usable characters → fallback, so a filename is never `findings-.md`.
    assert_eq!(sanitize_label_for_filename("///"), "task");
    // CJK (and other alphanumeric scripts) are preserved.
    assert_eq!(sanitize_label_for_filename("技术架构"), "技术架构");
    // Length is capped (by chars, UTF-8-safe) so a pathological label can't
    // overflow the filename; per-worker uniqueness comes from the index suffix
    // the caller appends, not from the full label.
    let long = "a".repeat(200);
    assert_eq!(sanitize_label_for_filename(&long).chars().count(), 48);
    let long_cjk = "中".repeat(100);
    assert_eq!(sanitize_label_for_filename(&long_cjk).chars().count(), 48);
}

#[test]
fn fanout_worker_deadline_priority_and_clamp() {
    // No deadline/timeout → absolute ceiling (never `None`, so a worker can
    // never hang forever).
    let bare = PipelineNode {
        id: "w".into(),
        ..Default::default()
    };
    assert_eq!(
        fanout_worker_deadline(&bare),
        Duration::from_secs(MAX_FANOUT_WORKER_SECS)
    );

    // `timeout_secs` is used when no `deadline_secs`.
    let timed = PipelineNode {
        id: "w".into(),
        timeout_secs: Some(42),
        ..Default::default()
    };
    assert_eq!(fanout_worker_deadline(&timed), Duration::from_secs(42));

    // `deadline_secs` WINS over `timeout_secs`.
    let both = PipelineNode {
        id: "w".into(),
        deadline_secs: Some(7.5),
        timeout_secs: Some(42),
        ..Default::default()
    };
    assert_eq!(fanout_worker_deadline(&both), Duration::from_secs_f64(7.5));

    // Non-finite / non-positive deadline_secs falls through to timeout_secs.
    let bad_deadline = PipelineNode {
        id: "w".into(),
        deadline_secs: Some(0.0),
        timeout_secs: Some(9),
        ..Default::default()
    };
    assert_eq!(
        fanout_worker_deadline(&bad_deadline),
        Duration::from_secs(9)
    );

    // A pathological over-cap value is clamped to the absolute ceiling.
    let over = PipelineNode {
        id: "w".into(),
        timeout_secs: Some(MAX_FANOUT_WORKER_SECS * 100),
        ..Default::default()
    };
    assert_eq!(
        fanout_worker_deadline(&over),
        Duration::from_secs(MAX_FANOUT_WORKER_SECS)
    );
}

#[test]
fn dag_schedulable_excludes_fanout_and_converge() {
    // Plain static graphs schedule on the DAG path.
    let linear = crate::parser::parse_dot(
        "digraph d { a [handler=codergen, tools=read_file]; \
             b [handler=codergen, tools=read_file]; a -> b }",
    )
    .unwrap();
    assert!(graph_is_dag_schedulable(&linear));

    // Runtime fan-out (converge + dynamic_parallel) needs the legacy walk.
    let fanout = crate::parser::parse_dot(
        "digraph p { s [handler=dynamic_parallel, prompt=\"plan\", converge=\"m\"]; \
             m [handler=codergen, tools=read_file]; s -> m }",
    )
    .unwrap();
    assert!(!graph_is_dag_schedulable(&fanout));

    // A normal retry loop (back-edge target `work` has a forward pred
    // `start`) is schedulable.
    let retry = crate::parser::parse_dot(
        "digraph r { start [handler=codergen, tools=read_file]; \
             work [handler=codergen, tools=read_file]; \
             check [handler=codergen, tools=read_file]; \
             start -> work; work -> check; \
             check -> work [label=\"back_edge\", condition=\"outcome.status == \\\"fail\\\"\"] }",
    )
    .unwrap();
    assert!(graph_is_dag_schedulable(&retry));

    // A back-edge-only target (`work` reachable solely via the back-edge,
    // no forward predecessor) is a spurious root → legacy.
    let spurious = crate::parser::parse_dot(
        "digraph b { start [handler=codergen, tools=read_file]; \
             check [handler=codergen, tools=read_file]; \
             work [handler=codergen, tools=read_file]; \
             start -> check; work -> check; \
             check -> work [label=\"back_edge\", condition=\"outcome.status == \\\"fail\\\"\"] }",
    )
    .unwrap();
    assert!(!graph_is_dag_schedulable(&spurious));

    // A retry edge back to the START node is valid (start is the root).
    let retry_to_start = crate::parser::parse_dot(
        "digraph s { start [handler=codergen, tools=read_file]; \
             check [handler=codergen, tools=read_file]; \
             start -> check; \
             check -> start [label=\"back_edge\", condition=\"outcome.status == \\\"fail\\\"\"] }",
    )
    .unwrap();
    assert!(graph_is_dag_schedulable(&retry_to_start));
}

#[test]
fn dag_forward_edge_fail_closed_on_unconditional_fail() {
    let pass = NodeOutcome {
        node_id: "x".into(),
        status: OutcomeStatus::Pass,
        content: String::new(),
        token_usage: TokenUsage::default(),
        files_modified: vec![],
    };
    let fail = NodeOutcome {
        status: OutcomeStatus::Fail,
        ..pass.clone()
    };
    let graph = crate::parser::parse_dot(
        "digraph g { x [handler=codergen, tools=read_file]; \
             y [handler=codergen, tools=read_file]; x -> y }",
    )
    .unwrap();
    let edge = &graph.edges[0];
    // Unconditional edge fires on Pass, NOT on Fail (fail-closed).
    assert!(dag_forward_edge_fires(&graph, edge, &pass, false).unwrap());
    assert!(!dag_forward_edge_fires(&graph, edge, &fail, false).unwrap());
}

#[test]
fn test_edge_selection_condition_match() {
    let graph = crate::parser::parse_dot(
        r#"
            digraph test {
                a [prompt="test"]
                b [prompt="test"]
                c [prompt="test"]
                a -> b [condition="outcome.status == \"pass\""]
                a -> c [condition="outcome.status == \"fail\""]
            }
            "#,
    )
    .unwrap();

    let executor = PipelineExecutor::new(make_test_config());
    let outcome = NodeOutcome {
        node_id: "a".into(),
        status: OutcomeStatus::Pass,
        content: String::new(),
        token_usage: TokenUsage::default(),
        files_modified: vec![],
    };

    let next = executor.select_next_edge(&graph, "a", &outcome).unwrap();
    assert_eq!(next, Some("b".into()));
}

#[test]
fn test_edge_selection_weight_tiebreak() {
    let graph = crate::parser::parse_dot(
        r#"
            digraph test {
                a -> b [weight="2.0"]
                a -> c [weight="1.0"]
            }
            "#,
    )
    .unwrap();

    let executor = PipelineExecutor::new(make_test_config());
    let outcome = NodeOutcome {
        node_id: "a".into(),
        status: OutcomeStatus::Pass,
        content: String::new(),
        token_usage: TokenUsage::default(),
        files_modified: vec![],
    };

    let next = executor.select_next_edge(&graph, "a", &outcome).unwrap();
    assert_eq!(next, Some("b".into()));
}

fn make_test_config() -> ExecutorConfig {
    // Minimal config for edge selection tests (doesn't actually run agents)
    ExecutorConfig {
        default_provider: Arc::new(MockProvider),
        provider_router: None,
        memory: Arc::new(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(create_test_store()),
        ),
        working_dir: PathBuf::from("/tmp"),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 8,
        max_pipeline_fanout_total: None,
        guards: Vec::new(),
        max_concurrent_llm_calls: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: crate::context::PipelineContext::default(),
        host_context: crate::host_context::PipelineHostContext::default(),
        embedder: None,
        catalog_dir: None,
        sandbox: octos_agent::SandboxConfig::default(),
    }
}

/// #1607 (codex-review follow-up): `run_terminal_validators` /
/// `run_node_validators` build their workspace-scoped validator registry
/// with `with_builtins_and_sandbox(&self.config.working_dir,
/// create_sandbox(&self.config.sandbox))`. Lock in that the sandbox
/// threaded onto `ExecutorConfig` reaches that registry (i.e. NOT the
/// pre-fix hardcoded `with_builtins` / `NoSandbox`). Docker mode is chosen
/// because `create_sandbox` returns a `DockerSandbox` unconditionally
/// (no docker binary required), so the assertion is host-independent.
#[test]
fn pipeline_threads_configured_sandbox_into_validator_registry() {
    let mut config = make_test_config();
    config.sandbox = octos_agent::SandboxConfig {
        mode: octos_agent::SandboxMode::Docker,
        ..octos_agent::SandboxConfig::default()
    };
    // Reconstruct exactly what the two validator blocks build.
    let registry = octos_agent::ToolRegistry::with_builtins_and_sandbox(
        &config.working_dir,
        octos_agent::create_sandbox(&config.sandbox),
    );
    let sandbox = registry.sandbox();
    assert!(
        sandbox.is_docker(),
        "pipeline validator registry must inherit the ExecutorConfig \
             sandbox (Docker here), not the pre-#1607 hardcoded NoSandbox"
    );
    assert!(
        !sandbox.is_noop(),
        "a real backend threaded onto ExecutorConfig must not be a no-op"
    );
}

/// #1607: an explicit `SandboxMode::None` on `ExecutorConfig` resolves to a
/// no-op backend, so command validators run the argv directly (pre-#1607
/// behaviour on a host without a configured backend). Note: the STRUCT
/// default is `SandboxMode::Auto`, which resolves to a REAL backend on
/// macOS/Linux — so this test pins `None` explicitly to stay
/// host-independent (mirrors `spawn_none_sandbox_registry_is_noop`).
#[test]
fn pipeline_none_sandbox_registry_is_noop() {
    let mut config = make_test_config();
    config.sandbox = octos_agent::SandboxConfig {
        mode: octos_agent::SandboxMode::None,
        ..octos_agent::SandboxConfig::default()
    };
    let registry = octos_agent::ToolRegistry::with_builtins_and_sandbox(
        &config.working_dir,
        octos_agent::create_sandbox(&config.sandbox),
    );
    assert!(
        registry.sandbox().is_noop(),
        "SandboxMode::None must resolve to a no-op backend so command \
             validators run directly (host-independent)"
    );
}

struct MockProvider;

#[async_trait::async_trait]
impl LlmProvider for MockProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                ..Default::default()
            },
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

struct CountingProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmProvider for CountingProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(octos_llm::ChatResponse {
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                ..Default::default()
            },
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

#[test]
fn validation_rejects_malformed_pipeline_before_llm_dispatch() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut config = make_test_config();
    config.default_provider = Arc::new(CountingProvider {
        calls: calls.clone(),
    });
    let executor = PipelineExecutor::new(config);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
        .block_on(executor.run(
            r#"
                digraph test {
                    start [prompt="Use {missing_runtime_binding}"]
                }
                "#,
            "input",
            &serde_json::Map::new(),
        ))
        .expect_err("unbound template variable must reject the pipeline");
    assert!(
        err.to_string().contains("T-Agent"),
        "unexpected validation error: {err}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "validation must fail before any LLM call"
    );
}

async fn create_test_store() -> EpisodeStore {
    let dir = tempfile::tempdir().unwrap();
    let dir = Box::leak(Box::new(dir));
    EpisodeStore::open(dir.path()).await.unwrap()
}

#[test]
fn defaults_pipeline_llm_throttle_to_four_permits() {
    let executor = PipelineExecutor::new(make_test_config());
    assert_eq!(
        executor.max_concurrent_llm_calls_for_test(),
        DEFAULT_PIPELINE_MAX_CONCURRENT_LLM_CALLS
    );
    assert_eq!(
        executor
            .build_codergen_for_test()
            .llm_available_permits_for_test(),
        Some(DEFAULT_PIPELINE_MAX_CONCURRENT_LLM_CALLS)
    );
}

#[test]
fn honors_configured_pipeline_llm_throttle_and_clamps_zero() {
    let mut config = make_test_config();
    config.max_concurrent_llm_calls = Some(2);
    let executor = PipelineExecutor::new(config);
    assert_eq!(executor.max_concurrent_llm_calls_for_test(), 2);
    assert_eq!(
        executor
            .build_codergen_for_test()
            .llm_available_permits_for_test(),
        Some(2)
    );

    let mut config = make_test_config();
    config.max_concurrent_llm_calls = Some(0);
    let executor = PipelineExecutor::new(config);
    assert_eq!(executor.max_concurrent_llm_calls_for_test(), 1);
    assert_eq!(
        executor
            .build_codergen_for_test()
            .llm_available_permits_for_test(),
        Some(1)
    );
}

#[test]
fn caps_codergen_output_tokens_by_remaining_pipeline_budget() {
    let mut node = PipelineNode {
        handler: HandlerKind::Codergen,
        ..Default::default()
    };
    cap_node_output_tokens_for_remaining_budget(&mut node, 900, 3);
    assert_eq!(node.max_output_tokens, Some(300));

    node.max_output_tokens = Some(100);
    cap_node_output_tokens_for_remaining_budget(&mut node, 900, 3);
    assert_eq!(node.max_output_tokens, Some(100));
}

#[test]
fn leaves_non_llm_nodes_uncapped_by_pipeline_budget() {
    let mut node = PipelineNode {
        handler: HandlerKind::Shell,
        max_output_tokens: Some(500),
        ..Default::default()
    };
    cap_node_output_tokens_for_remaining_budget(&mut node, 900, 3);
    assert_eq!(node.max_output_tokens, Some(500));
}

// --- extract_json_array tests ---

#[test]
fn test_extract_json_array_direct() {
    let input = r#"[{"task": "a", "label": "A"}]"#;
    assert_eq!(extract_json_array(input), Some(input));
}

#[test]
fn test_extract_json_array_with_code_fence() {
    let input = "```json\n[{\"task\": \"a\"}]\n```";
    assert_eq!(extract_json_array(input), Some("[{\"task\": \"a\"}]"));
}

#[test]
fn test_extract_json_array_with_narrative() {
    let input = "Here are [the angles] I recommend:\n[{\"task\": \"search\", \"label\": \"L\"}]";
    let result = extract_json_array(input).unwrap();
    assert!(result.starts_with("[{"));
    assert!(result.ends_with(']'));
}

#[test]
fn test_extract_json_array_no_array() {
    assert_eq!(extract_json_array("no json here"), None);
}

#[test]
fn test_extract_json_array_bare_brackets_no_object() {
    // Bare brackets without `{` should not match
    assert_eq!(extract_json_array("see [this] for details"), None);
}

#[test]
fn test_extract_json_array_whitespace() {
    let input = "  \n  [{\"task\": \"x\"}]  \n  ";
    assert_eq!(extract_json_array(input), Some("[{\"task\": \"x\"}]"));
}

// --- DynamicTask deserialization tests ---

#[test]
fn test_dynamic_task_full() {
    let json = r#"{"task": "search for X", "label": "Primary"}"#;
    let t: DynamicTask = serde_json::from_str(json).unwrap();
    assert_eq!(t.task, "search for X");
    assert_eq!(t.label.as_deref(), Some("Primary"));
}

#[test]
fn test_dynamic_task_no_label() {
    let json = r#"{"task": "search for Y"}"#;
    let t: DynamicTask = serde_json::from_str(json).unwrap();
    assert_eq!(t.task, "search for Y");
    assert!(t.label.is_none());
}

#[test]
fn test_dynamic_task_extra_fields_ignored() {
    let json = r#"{"task": "search", "label": "L", "extra": 42}"#;
    let t: DynamicTask = serde_json::from_str(json).unwrap();
    assert_eq!(t.task, "search");
}

#[test]
fn test_dynamic_task_array() {
    let json = r#"[{"task": "a", "label": "A"}, {"task": "b"}]"#;
    let tasks: Vec<DynamicTask> = serde_json::from_str(json).unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].task, "a");
    assert_eq!(tasks[1].label, None);
}

// --- fallback_tasks tests ---

#[test]
fn test_fallback_tasks_count() {
    let tasks = fallback_tasks("test query");
    assert_eq!(tasks.len(), 3);
    assert!(tasks.iter().all(|t| t.label.is_some()));
    assert!(tasks[0].task.contains("test query"));
}

/// Build a fresh ExecutorConfig identical to `make_test_config` but
/// with a per-test cumulative fan-out cap so Guard B fires on a
/// small synthetic graph instead of waiting for 500 dispatches.
async fn make_capped_config(cap: usize) -> ExecutorConfig {
    ExecutorConfig {
        default_provider: Arc::new(MockProvider),
        provider_router: None,
        memory: Arc::new(create_test_store().await),
        working_dir: PathBuf::from("/tmp"),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 8,
        max_pipeline_fanout_total: Some(cap),
        guards: Vec::new(),
        max_concurrent_llm_calls: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: crate::context::PipelineContext::default(),
        host_context: crate::host_context::PipelineHostContext::default(),
        embedder: None,
        catalog_dir: None,
        sandbox: octos_agent::SandboxConfig::default(),
    }
}

struct TokenHandler {
    calls: Arc<AtomicUsize>,
    input_tokens: u32,
    output_tokens: u32,
}

#[async_trait::async_trait]
impl Handler for TokenHandler {
    async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
        self.calls.fetch_add(1, AtomicOrdering::Relaxed);
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: OutcomeStatus::Pass,
            content: format!("{} complete", node.id),
            token_usage: TokenUsage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                ..Default::default()
            },
            files_modified: vec![],
        })
    }
}

struct NodeRecordingGuard {
    seen: Arc<Mutex<Vec<String>>>,
}

impl PipelineGuard for NodeRecordingGuard {
    fn before_node(&self, ctx: &GuardContext<'_>) -> Result<GuardDecision> {
        self.seen.lock().unwrap().push(ctx.node.id.clone());
        Ok(GuardDecision::Allow)
    }
}

struct SelectiveGuard {
    target: &'static str,
    decision: GuardDecision,
}

impl PipelineGuard for SelectiveGuard {
    fn before_node(&self, ctx: &GuardContext<'_>) -> Result<GuardDecision> {
        if ctx.node.id == self.target {
            Ok(self.decision.clone())
        } else {
            Ok(GuardDecision::Allow)
        }
    }
}

struct OrderedGuard {
    name: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
    decision: GuardDecision,
}

impl PipelineGuard for OrderedGuard {
    fn before_node(&self, ctx: &GuardContext<'_>) -> Result<GuardDecision> {
        self.seen.lock().unwrap().push(format!(
            "{}:{}:{}:{}:{}",
            self.name,
            ctx.node.id,
            ctx.cumulative_tokens,
            ctx.completed_count,
            ctx.visit_counts
                .get(&ctx.node.id)
                .copied()
                .unwrap_or_default()
        ));
        Ok(self.decision.clone())
    }
}

struct ErrorGuard;

impl PipelineGuard for ErrorGuard {
    fn before_node(&self, _ctx: &GuardContext<'_>) -> Result<GuardDecision> {
        Err(eyre::eyre!("guard storage unavailable"))
    }
}

#[tokio::test]
async fn token_budget_guard_aborts_before_next_node_with_partial_result() {
    let mut config = make_capped_config(10).await;
    config.guards = vec![Arc::new(TokenBudgetGuard::new(7)) as Arc<dyn PipelineGuard>];
    let executor = PipelineExecutor::new(config);

    let calls = Arc::new(AtomicUsize::new(0));
    let mut handlers = HandlerRegistry::new();
    handlers.register(
        HandlerKind::Noop,
        Arc::new(TokenHandler {
            calls: calls.clone(),
            input_tokens: 4,
            output_tokens: 3,
        }),
    );

    let dot = r#"
            digraph t {
                a [handler="noop"]
                b [handler="noop"]
                a -> b
            }
        "#;

    let result = executor
        .run_with_handlers(dot, "seed", &serde_json::Map::new(), handlers)
        .await
        .expect("pipeline should return a partial result");

    assert!(!result.success);
    assert!(
        result
            .output
            .contains("token budget exhausted before node 'b'"),
        "unexpected output: {}",
        result.output
    );
    assert_eq!(calls.load(AtomicOrdering::Relaxed), 1);
    assert_eq!(result.node_summaries.len(), 1);
    assert_eq!(result.node_summaries[0].node_id, "a");
    assert_eq!(result.token_usage.input_tokens, 4);
    assert_eq!(result.token_usage.output_tokens, 3);
}

#[tokio::test]
async fn timeout_guard_aborts_before_dispatch() {
    let mut config = make_capped_config(10).await;
    config.guards = vec![Arc::new(TimeoutGuard::new(Duration::ZERO)) as Arc<dyn PipelineGuard>];
    let executor = PipelineExecutor::new(config);

    let result = executor
        .run(
            r#"digraph t { a [handler="noop"] }"#,
            "seed",
            &serde_json::Map::new(),
        )
        .await
        .expect("timeout guard should return a partial result");

    assert!(!result.success);
    assert!(result.node_summaries.is_empty());
    assert!(
        result.output.contains("pipeline timeout before node 'a'"),
        "unexpected output: {}",
        result.output
    );
}

#[tokio::test]
async fn guard_skip_records_fail_outcome_for_edge_routing() {
    let mut config = make_capped_config(10).await;
    config.guards = vec![Arc::new(SelectiveGuard {
        target: "gate",
        decision: GuardDecision::Skip("closed by policy".into()),
    }) as Arc<dyn PipelineGuard>];
    let executor = PipelineExecutor::new(config);

    let dot = r#"
            digraph t {
                gate [handler="noop"]
                fallback [handler="noop"]
                bad [handler="noop"]
                gate -> fallback [condition="outcome.status == \"fail\""]
                gate -> bad [condition="outcome.status == \"pass\""]
            }
        "#;

    let result = executor
        .run(dot, "seed", &serde_json::Map::new())
        .await
        .expect("guard skip should route to fallback");

    assert!(result.success, "fallback noop should recover the pipeline");
    assert_eq!(result.node_summaries[0].node_id, "gate");
    assert!(!result.node_summaries[0].success);
    assert!(
        result
            .node_summaries
            .iter()
            .any(|s| s.node_id == "fallback")
    );
    assert!(!result.node_summaries.iter().any(|s| s.node_id == "bad"));
    assert!(
        result
            .output
            .contains("Node 'gate' skipped by pipeline guard: closed by policy"),
        "unexpected output: {}",
        result.output
    );
}

#[tokio::test]
async fn guards_run_in_registration_order_and_short_circuit() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut config = make_capped_config(10).await;
    config.guards = vec![
        Arc::new(OrderedGuard {
            name: "first",
            seen: seen.clone(),
            decision: GuardDecision::Allow,
        }) as Arc<dyn PipelineGuard>,
        Arc::new(OrderedGuard {
            name: "second",
            seen: seen.clone(),
            decision: GuardDecision::Abort("stop here".into()),
        }) as Arc<dyn PipelineGuard>,
        Arc::new(OrderedGuard {
            name: "third",
            seen: seen.clone(),
            decision: GuardDecision::Allow,
        }) as Arc<dyn PipelineGuard>,
    ];
    let executor = PipelineExecutor::new(config);

    let result = executor
        .run(
            r#"digraph t { a [handler="noop"] }"#,
            "seed",
            &serde_json::Map::new(),
        )
        .await
        .expect("guard abort should return partial result");

    assert!(!result.success);
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["first:a:0:0:1".to_string(), "second:a:0:0:1".to_string()]
    );
}

#[tokio::test]
async fn guard_errors_abort_instead_of_allowing_dispatch() {
    let mut config = make_capped_config(10).await;
    config.guards = vec![Arc::new(ErrorGuard) as Arc<dyn PipelineGuard>];
    let executor = PipelineExecutor::new(config);

    let calls = Arc::new(AtomicUsize::new(0));
    let mut handlers = HandlerRegistry::new();
    handlers.register(
        HandlerKind::Noop,
        Arc::new(TokenHandler {
            calls: calls.clone(),
            input_tokens: 0,
            output_tokens: 0,
        }),
    );

    let result = executor
        .run_with_handlers(
            r#"digraph t { a [handler="noop"] }"#,
            "seed",
            &serde_json::Map::new(),
            handlers,
        )
        .await
        .expect("guard error should return partial result");

    assert!(!result.success);
    assert_eq!(calls.load(AtomicOrdering::Relaxed), 0);
    assert!(
        result
            .output
            .contains("guard error: guard storage unavailable"),
        "unexpected output: {}",
        result.output
    );
}

#[tokio::test]
async fn guards_run_once_for_static_parallel_before_worker_spawn() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut config = make_capped_config(10).await;
    config.guards =
        vec![Arc::new(NodeRecordingGuard { seen: seen.clone() }) as Arc<dyn PipelineGuard>];
    let executor = PipelineExecutor::new(config);

    let dot = r#"
            digraph t {
                fan [handler="parallel", converge="merge"]
                a [handler="noop"]
                b [handler="noop"]
                merge [handler="noop"]
                fan -> a
                fan -> b
                a -> merge
                b -> merge
            }
        "#;

    let result = executor
        .run(dot, "seed", &serde_json::Map::new())
        .await
        .expect("parallel pipeline should complete");
    assert!(result.success);

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.iter().filter(|node| node.as_str() == "fan").count(), 1);
    assert!(!seen.iter().any(|node| node == "a"));
    assert!(!seen.iter().any(|node| node == "b"));
    assert!(seen.iter().any(|node| node == "merge"));
}

#[tokio::test]
async fn guards_run_once_for_dynamic_parallel_before_worker_spawn() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut config = make_capped_config(10).await;
    config.guards =
        vec![Arc::new(NodeRecordingGuard { seen: seen.clone() }) as Arc<dyn PipelineGuard>];
    let executor = PipelineExecutor::new(config);

    let mut handlers = HandlerRegistry::new();
    handlers.register(HandlerKind::Codergen, Arc::new(NoopHandler));
    handlers.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));
    handlers.register(HandlerKind::Noop, Arc::new(NoopHandler));

    let dot = r#"
            digraph t {
                plan [handler="dynamic_parallel", converge="merge", prompt="plan"]
                merge [handler="noop"]
                plan -> merge
            }
        "#;

    let result = executor
        .run_with_handlers(dot, "seed", &serde_json::Map::new(), handlers)
        .await
        .expect("dynamic parallel pipeline should complete");
    assert!(result.success);

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter().filter(|node| node.as_str() == "plan").count(),
        1
    );
    assert!(!seen.iter().any(|node| node.starts_with("plan_task_")));
    assert!(seen.iter().any(|node| node == "merge"));
}

/// Guard B regression: a `dynamic_parallel` node whose worker count
/// exceeds the cumulative fan-out cap must fail the pipeline with
/// `PipelineError::FanoutExceeded` before any worker dispatches.
/// The test forces the planner to fall back to the 3-task fallback
/// (the `MockProvider` returns plain "done" which fails JSON
/// extraction) and sets the cap to 2 so the fan-out trips.
#[tokio::test]
async fn dynamic_parallel_fails_after_cumulative_cap() {
    let config = make_capped_config(2).await;
    let executor = PipelineExecutor::new(config);

    // Minimal dynamic_parallel graph. The planner is the
    // MockProvider, which returns content "done" — that fails JSON
    // extraction and routes through the 3-task fallback. With
    // cap=2 the fan-out gate refuses before any worker dispatches.
    let dot = r#"
            digraph t {
                plan [handler="dynamic_parallel", converge="merge", prompt="plan"]
                merge [handler="noop"]
                plan -> merge
            }
        "#;

    let result = executor
        .run(dot, "drive a runaway plan", &serde_json::Map::new())
        .await;

    let Err(error) = result else {
        panic!("expected pipeline to fail at the fan-out cap; got {result:?}");
    };
    // The structured `PipelineError::FanoutExceeded` is wrapped in
    // an `eyre::Report` — downcast to assert the typed reason.
    let typed = error
        .downcast_ref::<PipelineError>()
        .expect("expected PipelineError variant in failure chain");
    match typed {
        PipelineError::FanoutExceeded { count, cap } => {
            assert_eq!(*cap, 2, "cap should match the per-test override");
            assert_eq!(*count, 0, "no workers should dispatch before the cap fires");
        }
    }
}

/// Planner provider for the dynamic fan-out concurrency test: returns a
/// JSON array of exactly 6 tasks so the `dynamic_parallel` node plans
/// MORE workers than `max_parallel_workers`.
struct SixTaskPlanner;

#[async_trait::async_trait]
impl LlmProvider for SixTaskPlanner {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some(
                r#"[{"task":"t1","label":"T1"},{"task":"t2","label":"T2"},
                        {"task":"t3","label":"T3"},{"task":"t4","label":"T4"},
                        {"task":"t5","label":"T5"},{"task":"t6","label":"T6"}]"#
                    .to_string(),
            ),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock-planner"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Codergen stand-in that records the peak number of concurrently
/// executing fan-out workers: increment an in-flight counter on entry,
/// fold it into the running maximum, hold the slot across a real await,
/// decrement on exit.
struct ConcurrencyProbeHandler {
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Handler for ConcurrencyProbeHandler {
    async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
        let now = self.in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, AtomicOrdering::SeqCst);
        // Hold the slot across an await point. `join_all` polls every
        // worker future once within microseconds, so 50ms guarantees all
        // concurrently-dispatched workers pile up before the first exits
        // — no racing needed for a deterministic peak.
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
        self.executed.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: OutcomeStatus::Pass,
            content: format!("{} done", node.id),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        })
    }
}

/// The dynamic fan-out must honor `max_parallel_workers` exactly like
/// the static `Parallel` branch. A planner that yields 6 tasks with
/// `max_parallel_workers = 2` may never have more than 2 workers
/// in flight at once.
#[tokio::test]
async fn should_cap_dynamic_parallel_worker_concurrency_when_planner_exceeds_limit() {
    let mut config = make_capped_config(100).await;
    config.default_provider = Arc::new(SixTaskPlanner);
    config.max_parallel_workers = 2;
    let executor = PipelineExecutor::new(config);

    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));

    let mut handlers = HandlerRegistry::new();
    handlers.register(
        HandlerKind::Codergen,
        Arc::new(ConcurrencyProbeHandler {
            in_flight: in_flight.clone(),
            max_in_flight: max_in_flight.clone(),
            executed: executed.clone(),
        }),
    );
    handlers.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));
    handlers.register(HandlerKind::Noop, Arc::new(NoopHandler));

    let dot = r#"
            digraph t {
                plan [handler="dynamic_parallel", converge="merge", prompt="plan"]
                merge [handler="noop"]
                plan -> merge
            }
        "#;

    let result = executor
        .run_with_handlers(dot, "seed", &serde_json::Map::new(), handlers)
        .await
        .expect("dynamic parallel pipeline should complete");
    assert!(result.success);

    // The planner JSON must have driven the fan-out (6 workers), not the
    // 3-task fallback — otherwise the concurrency assertion below tests
    // a weaker shape than intended.
    assert_eq!(
        executed.load(AtomicOrdering::SeqCst),
        6,
        "expected all 6 planned workers to run"
    );
    let peak = max_in_flight.load(AtomicOrdering::SeqCst);
    assert!(
        peak <= 2,
        "dynamic_parallel fan-out must gate workers to max_parallel_workers=2, \
             but {peak} ran concurrently"
    );
}

/// Guard B sanity check: when the fan-out is below the cap the
/// pipeline executes normally. Static `Parallel` graph with two
/// noop targets and cap=4 — well within budget.
#[tokio::test]
async fn parallel_under_cap_runs_to_completion() {
    let config = make_capped_config(4).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
            digraph t {
                fan [handler="parallel", converge="merge"]
                a [handler="noop"]
                b [handler="noop"]
                merge [handler="noop"]
                fan -> a
                fan -> b
                a -> merge
                b -> merge
            }
        "#;

    let result = executor
        .run(dot, "happy path", &serde_json::Map::new())
        .await;
    assert!(
        result.is_ok(),
        "fan-out below cap should complete: {result:?}"
    );
}

// ── L2 typed-IR execution (S1-3) ───────────────────────────────────

/// A composed typed-IR program executes through `run_graph_with_handlers`
/// without ever round-tripping through DOT text.
#[tokio::test]
async fn run_ir_executes_composed_graph_without_dot_roundtrip() {
    let executor = PipelineExecutor::new(make_capped_config(4).await);
    let ir = r#"{"id":"p","nodes":[{"id":"g","kind":{"type":"gate"}}]}"#;
    let result = executor
        .run_ir(
            ir,
            &crate::profile::ValidationProfile::l2_default(),
            "hi",
            &serde_json::Map::new(),
        )
        .await;
    assert!(result.is_ok(), "composed gate graph should run: {result:?}");
}

/// Compose-time failures surface as an error before any execution begins.
#[tokio::test]
async fn run_ir_surfaces_compose_errors_before_execution() {
    let executor = PipelineExecutor::new(make_capped_config(4).await);
    let bad = r#"{"id":"p","nodes":[{"id":"n","kind":{"type":"shell"}}]}"#;
    let err = executor
        .run_ir(
            bad,
            &crate::profile::ValidationProfile::l2_default(),
            "x",
            &serde_json::Map::new(),
        )
        .await
        .expect_err("unknown palette kind must fail at compose");
    assert!(err.to_string().contains("compose"), "got: {err}");
}

/// Real END-TO-END: compile an L2 typed-IR "deep research" workflow and
/// EXECUTE it against a live DeepSeek model (not MockProvider). Env-gated +
/// `#[ignore]` so normal CI skips it. Run with:
///   DEEPSEEK_API_KEY=... cargo test -p octos-pipeline \
///     run_ir_e2e_deepseek_real -- --ignored --nocapture
#[tokio::test]
#[ignore = "needs DEEPSEEK_API_KEY + network"]
async fn run_ir_e2e_deepseek_real() {
    let Ok(key) = std::env::var("DEEPSEEK_API_KEY") else {
        eprintln!("SKIP: DEEPSEEK_API_KEY not set");
        return;
    };
    let provider: Arc<dyn LlmProvider> = Arc::new(
        octos_llm::openai::OpenAIProvider::new(key, "deepseek-chat")
            .with_base_url("https://api.deepseek.com/v1"),
    );
    let mut config = make_capped_config(4).await;
    config.default_provider = provider;
    let executor = PipelineExecutor::new(config);

    // A small but real "deep research" IR: research -> synthesize report.
    let ir = r#"{
            "id": "deep_research_e2e",
            "nodes": [
                {"id":"research","kind":{"type":"research","prompt":"Briefly research the current state of Rust async runtimes (Tokio, smol, io_uring runtimes). List 4-5 concrete factual points. Keep it under 120 words."}},
                {"id":"report","kind":{"type":"synthesize","prompt":"Using the prior research findings, write a tight ~150-word summary report with a title, in markdown."}}
            ],
            "edges": [ {"source":"research","target":"report"} ]
        }"#;

    let result = executor
        .run_ir(
            ir,
            &crate::profile::ValidationProfile::l2_default(),
            "Rust async runtimes, 2026",
            &serde_json::Map::new(),
        )
        .await;

    match &result {
        Ok(r) => {
            eprintln!(
                "=== e2e success={} nodes_run={} ===",
                r.success,
                r.node_summaries.len()
            );
            eprintln!("=== FINAL OUTPUT ===\n{}\n=== END ===", r.output);
        }
        Err(e) => eprintln!("=== e2e ERROR ===\n{e:?}"),
    }
    let r = result.expect("composed IR pipeline should execute end-to-end");
    assert!(r.success, "pipeline should succeed");
    assert!(!r.output.trim().is_empty(), "should produce report output");
}

// ── Heartbeat (#964 follow-up) ─────────────────────────────────────
//
// Verifies that `spawn_pipeline_heartbeat` ticks at the configured
// interval, reads the shared `PipelineStatusSnapshot` each tick, and
// emits `ProgressEvent::ToolProgress` events through the captured
// reporter. The guard's `Drop` aborts the task so it doesn't outlive
// the surrounding `run_with_handlers` call.

/// Capturing reporter — collects every emitted `ProgressEvent` into a
/// `Vec` so the test can assert on the messages.
#[derive(Default, Clone)]
struct CapturingReporter {
    events: Arc<std::sync::Mutex<Vec<octos_agent::progress::ProgressEvent>>>,
}

impl octos_agent::progress::ProgressReporter for CapturingReporter {
    fn report(&self, event: octos_agent::progress::ProgressEvent) {
        if let Ok(mut g) = self.events.lock() {
            g.push(event);
        }
    }
}

#[tokio::test]
async fn heartbeat_emits_periodic_progress_with_current_node() {
    let reporter = CapturingReporter::default();
    let captured = reporter.events.clone();

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-heartbeat".to_string(),
        reporter: Arc::new(reporter),
        ..octos_agent::tools::ToolContext::zero()
    };

    let status = Arc::new(std::sync::Mutex::new(PipelineStatusSnapshot {
        pipeline_id: "research".to_string(),
        current_node: "plan_and_search".to_string(),
        nodes_done: 0,
        nodes_total: 3,
        start: Instant::now(),
    }));

    // Run the heartbeat inside TOOL_CTX.scope so the spawn helper can
    // capture reporter + tool_id synchronously. The 1s interval keeps
    // the test fast while still proving the periodic shape.
    let status_for_advance = status.clone();
    TOOL_CTX
        .scope(ctx, async move {
            let _guard = spawn_pipeline_heartbeat(status_for_advance.clone(), 1)
                .expect("heartbeat should spawn when TOOL_CTX is set");
            // Wait long enough for ≥2 ticks: first tick is consumed
            // by `interval.tick().await` (the skip-immediate guard),
            // the next two fire at +1s and +2s. Sleep 2.4s real time.
            tokio::time::sleep(Duration::from_millis(2_400)).await;

            // Update the snapshot mid-flight so the next tick
            // reflects the new node — guards against a stale snapshot
            // baked at spawn time.
            if let Ok(mut g) = status_for_advance.lock() {
                g.current_node = "analyze".to_string();
                g.nodes_done = 1;
            }
            tokio::time::sleep(Duration::from_millis(1_100)).await;
            // Guard drops here — heartbeat task aborts.
        })
        .await;

    let events = captured.lock().unwrap();
    // Expect ≥2 ticks (sleep 2.4s skips first immediate tick, then
    // fires at +1s and +2s) plus possibly +3.5s for the post-update
    // tick. Lower bound: 2.
    assert!(
        events.len() >= 2,
        "expected ≥2 heartbeat events in 3.5s; got {}: {:?}",
        events.len(),
        events,
    );

    let messages: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            octos_agent::progress::ProgressEvent::ToolProgress { message, .. } => {
                Some(message.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        messages.len(),
        events.len(),
        "heartbeat must emit ToolProgress events only — got: {events:?}",
    );

    let combined = messages.join("\n");
    assert!(
        combined.contains("research"),
        "heartbeat must include the pipeline id; got: {combined}",
    );
    assert!(
        combined.contains("plan_and_search") || combined.contains("analyze"),
        "heartbeat must surface the current_node from the snapshot; got: {combined}",
    );
    // Each tick should also include an elapsed-seconds suffix so
    // every message is unique — protects against SPA dedup-by-message
    // that would otherwise collapse identical chips.
    assert!(
        combined.contains("s elapsed"),
        "heartbeat message must contain '<N>s elapsed'; got: {combined}",
    );
}

#[tokio::test]
async fn heartbeat_guard_drop_stops_emission() {
    let reporter = CapturingReporter::default();
    let captured = reporter.events.clone();

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-heartbeat-stop".to_string(),
        reporter: Arc::new(reporter),
        ..octos_agent::tools::ToolContext::zero()
    };

    let status = Arc::new(std::sync::Mutex::new(PipelineStatusSnapshot {
        pipeline_id: "p".to_string(),
        current_node: "n".to_string(),
        nodes_done: 0,
        nodes_total: 1,
        start: Instant::now(),
    }));

    TOOL_CTX
            .scope(ctx, async move {
                {
                    let _guard = spawn_pipeline_heartbeat(status.clone(), 1).unwrap();
                    tokio::time::sleep(Duration::from_millis(1_200)).await;
                    // _guard drops here when block exits.
                }
                let count_at_drop = captured.lock().unwrap().len();
                // Sleep past 2 more theoretical tick intervals.
                tokio::time::sleep(Duration::from_millis(2_500)).await;
                let count_after_drop = captured.lock().unwrap().len();
                assert_eq!(
                    count_at_drop, count_after_drop,
                    "no new heartbeat events should fire after the guard drops; got {count_at_drop} -> {count_after_drop}",
                );
            })
            .await;
}

// ── Gap 4.2: structured per-node progress + ETA + previews ─────────

/// Linear ETA: `(elapsed / done) * remaining`, with graceful degradation.
#[test]
fn linear_eta_degrades_then_extrapolates() {
    // 0 nodes done → no rate yet → "estimating…" (None).
    assert_eq!(linear_eta_secs(30, 0, 3), None);
    // total 0 (degenerate) → None.
    assert_eq!(linear_eta_secs(30, 0, 0), None);
    // 1 of 3 done in 30s → 30s/node × 2 remaining = 60s.
    assert_eq!(linear_eta_secs(30, 1, 3), Some(60));
    // 2 of 4 done in 40s → 20s/node × 2 remaining = 40s.
    assert_eq!(linear_eta_secs(40, 2, 4), Some(40));
    // last node done / over-count → None (nothing left to estimate).
    assert_eq!(linear_eta_secs(90, 3, 3), None);
    assert_eq!(linear_eta_secs(90, 4, 3), None);

    // Monotone-ish sanity: as more nodes complete at a steady rate, the
    // ETA decreases (or holds), never increases.
    let mut prev = u64::MAX;
    for done in 1..5usize {
        // steady 10s/node.
        let elapsed = (done as u64) * 10;
        if let Some(eta) = linear_eta_secs(elapsed, done, 5) {
            assert!(
                eta <= prev,
                "ETA must not grow as nodes complete at a steady rate: {eta} > {prev}"
            );
            prev = eta;
        }
    }
}

/// A huge node output must yield a small, bounded preview (Gap-3.4 reuse).
#[test]
fn node_output_preview_is_bounded() {
    let huge = "x".repeat(500_000);
    let preview = node_output_preview(&huge);
    assert!(
        preview.len() <= NODE_PREVIEW_MAX_CHARS + 64,
        "preview must be bounded near the cap; got {} bytes",
        preview.len()
    );
    assert!(
        preview.contains("[truncated]"),
        "a truncated preview must carry the Gap-3.4 marker; got: {}",
        &preview[..preview.len().min(80)]
    );
    // A small output passes through unbounded (no false truncation).
    let small = node_output_preview("short answer");
    assert_eq!(small, "short answer");
}

/// NodeStarted + NodeCompleted each emit a structured `octos.harness
/// .event.v1` Progress event with node name + N/M (+ preview + success on
/// completed). RED before the executor wired the harness sink: only an
/// opaque heartbeat existed.
#[tokio::test]
async fn node_started_and_completed_emit_structured_harness_events() {
    use octos_agent::harness_events::{
        HarnessEvent, HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
    };

    // A real on-disk sink + registered context so the emit helper resolves
    // session/task ids and writes a v1 event line.
    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-pipeline-gap42".to_string(),
        },
    );

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-pipeline-gap42".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    let sink_for_assert = sink_uri.clone();
    TOOL_CTX
        .scope(ctx, async move {
            // node 2 of 3 starts.
            emit_pipeline_node_event(
                "research",
                "node_started",
                "analyze (2 of 3)",
                "analyze",
                2,
                3,
                None,
                None,
            );
            // node 2 of 3 completes with a bounded preview.
            let preview = node_output_preview(&"y".repeat(100_000));
            emit_pipeline_node_event(
                "research",
                "node_completed",
                "analyze (2 of 3) — done",
                "analyze",
                2,
                3,
                Some(true),
                Some(&preview),
            );
        })
        .await;

    detach_event_sink_context(&sink_uri);

    let lines = std::fs::read_to_string(&sink_for_assert).expect("read sink");
    let events: Vec<HarnessEvent> = lines
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| HarnessEvent::from_json_line(l).expect("valid harness event"))
        .collect();
    assert_eq!(events.len(), 2, "expected NodeStarted + NodeCompleted");

    // Both are Progress events on the v1 contract carrying the structured
    // node fields the consumers render (via runtime_detail_value).
    let started = events[0].runtime_detail_value(None, None);
    assert_eq!(started["kind"], "progress");
    assert_eq!(started["workflow_kind"], "research");
    assert_eq!(started["phase"], "node_started");
    assert_eq!(started["node"], "analyze");
    assert_eq!(started["node_index"], 2);
    assert_eq!(started["node_total"], 3);
    assert!(
        started["progress_message"]
            .as_str()
            .unwrap()
            .contains("2 of 3")
    );

    let completed = events[1].runtime_detail_value(None, None);
    assert_eq!(completed["phase"], "node_completed");
    assert_eq!(completed["node"], "analyze");
    assert_eq!(completed["success"], true);
    let preview = completed["preview"].as_str().expect("preview field");
    assert!(
        preview.len() <= NODE_PREVIEW_MAX_CHARS + 64,
        "completed preview must be bounded; got {} bytes",
        preview.len()
    );
    // The whole event line must stay well under the harness line cap.
    let line_len = serde_json::to_string(&events[1]).unwrap().len();
    assert!(
        line_len < octos_agent::harness_events::MAX_HARNESS_EVENT_LINE_BYTES,
        "structured progress event must stay under the line cap; got {line_len}"
    );
}

/// The heartbeat carries the linear ETA (and "estimating…" when 0 done).
#[tokio::test]
async fn heartbeat_carries_eta_label() {
    let reporter = CapturingReporter::default();
    let captured = reporter.events.clone();

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-heartbeat-eta".to_string(),
        reporter: Arc::new(reporter),
        ..octos_agent::tools::ToolContext::zero()
    };

    // Start with 0 done so the first ticks read "estimating…", then flip
    // to 1-of-3 so a later tick extrapolates an ETA.
    let status = Arc::new(std::sync::Mutex::new(PipelineStatusSnapshot {
        pipeline_id: "research".to_string(),
        current_node: "plan".to_string(),
        nodes_done: 0,
        nodes_total: 3,
        start: Instant::now(),
    }));

    let status_for_advance = status.clone();
    TOOL_CTX
        .scope(ctx, async move {
            let _guard = spawn_pipeline_heartbeat(status_for_advance.clone(), 1)
                .expect("heartbeat should spawn");
            tokio::time::sleep(Duration::from_millis(1_200)).await;
            if let Ok(mut g) = status_for_advance.lock() {
                g.nodes_done = 1;
                g.current_node = "analyze".to_string();
            }
            tokio::time::sleep(Duration::from_millis(2_200)).await;
        })
        .await;

    let messages: Vec<String> = captured
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            octos_agent::progress::ProgressEvent::ToolProgress { message, .. } => {
                Some(message.clone())
            }
            _ => None,
        })
        .collect();
    let combined = messages.join("\n");
    assert!(
        combined.contains("estimating…"),
        "heartbeat must say 'estimating…' before any node completes; got: {combined}"
    );
    assert!(
        combined.contains("s left"),
        "heartbeat must surface an ETA once ≥1 node completes; got: {combined}"
    );
}

/// Phase 2-A integration — the `working_dir` set on
/// [`ExecutorConfig`] must flow all the way down through
/// [`PipelineExecutor::build_codergen`] onto the per-node
/// [`CodergenHandler`]'s `working_dir`. This is the wire that
/// `RunPipelineTool::execute` rides when it swaps the tool's
/// pinned working dir for `scope.workspace()`. If this regresses,
/// the mini5 NEW-06 fix silently goes dead even though the
/// resolver still computes the right CWD.
///
/// `make_test_config` opens its own runtime so it can't be called
/// from inside `#[tokio::test]`; we mirror the `make_capped_config`
/// pattern (async test + async config builder) so we share the
/// outer runtime.
#[tokio::test]
async fn build_codergen_propagates_executor_working_dir_to_handler() {
    let custom_wd = tempfile::tempdir().expect("temp dir");
    let mut config = ExecutorConfig {
        default_provider: Arc::new(MockProvider),
        provider_router: None,
        memory: Arc::new(create_test_store().await),
        working_dir: PathBuf::from("/tmp"),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 8,
        max_pipeline_fanout_total: None,
        guards: Vec::new(),
        max_concurrent_llm_calls: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: crate::context::PipelineContext::default(),
        host_context: crate::host_context::PipelineHostContext::default(),
        embedder: None,
        catalog_dir: None,
        sandbox: octos_agent::SandboxConfig::default(),
    };
    config.working_dir = custom_wd.path().to_path_buf();
    let executor = PipelineExecutor::new(config);
    let codergen = executor.build_codergen_for_test();
    assert_eq!(
        codergen.working_dir_for_test(),
        custom_wd.path(),
        "CodergenHandler must inherit ExecutorConfig.working_dir so the \
             Phase 2-A scope override actually reaches per-node worker CWDs"
    );
}

/// Phase 2-A codex review (#1203) — when the pipeline runs inside a
/// session, the worker CWD (`working_dir`) and the catalog/profile
/// root MUST be separable. The executor's model assignment pass
/// reads `pipeline_models.json` / `model_catalog.json` from the
/// profile data dir, not the per-session workspace. Without the
/// split, scoped runs would silently lose strong/fast model
/// defaults and cost projections would fall back to the minimum
/// estimate. Pin the split: with `catalog_dir` populated, catalog
/// reads resolve against it even though `working_dir` was swapped.
#[tokio::test]
async fn catalog_dir_overrides_working_dir_for_model_assignment() {
    let profile_root = tempfile::tempdir().expect("profile root");
    let session_workspace = tempfile::tempdir().expect("session workspace");

    // Write a minimal catalog only under the profile root. If the
    // assignment pass reads from working_dir (the session
    // workspace) it will find nothing and silently no-op; if it
    // reads from catalog_dir (the profile root) it will load the
    // file.
    let pipeline_models = profile_root.path().join("pipeline_models.json");
    std::fs::write(&pipeline_models, b"{\"strong\":[],\"fast\":[]}").unwrap();

    let config = ExecutorConfig {
        default_provider: Arc::new(MockProvider),
        provider_router: None,
        memory: Arc::new(create_test_store().await),
        // worker CWD = per-session workspace (what Phase 2-A
        // overrides onto when a scope is present).
        working_dir: session_workspace.path().to_path_buf(),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 8,
        max_pipeline_fanout_total: None,
        guards: Vec::new(),
        max_concurrent_llm_calls: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: crate::context::PipelineContext::default(),
        host_context: crate::host_context::PipelineHostContext::default(),
        embedder: None,
        // catalog reads must hit the PROFILE root, not the worker CWD.
        catalog_dir: Some(profile_root.path().to_path_buf()),
        sandbox: octos_agent::SandboxConfig::default(),
    };

    // Pin the helper that the executor uses for catalog lookup:
    // unwrap_or-fallback must yield the catalog_dir when set.
    let executor = PipelineExecutor::new(config);
    let catalog_dir = executor
        .config
        .catalog_dir
        .as_deref()
        .unwrap_or(&executor.config.working_dir);
    assert_eq!(
        catalog_dir,
        profile_root.path(),
        "catalog_dir must be preferred over working_dir for catalog reads — \
             scoped runs lose model defaults without this split (codex #1203 P2)"
    );
    assert_ne!(
        catalog_dir, executor.config.working_dir,
        "the test setup must actually exercise the split path \
             (catalog_dir != working_dir)"
    );
}

/// Backward-compat — when `catalog_dir` is `None` (legacy callers
/// that didn't opt into the split), catalog reads still resolve
/// against `working_dir`. This is exactly the pre-Phase-2-A path.
#[tokio::test]
async fn catalog_dir_falls_back_to_working_dir_when_unset() {
    let only_dir = tempfile::tempdir().expect("temp dir");
    let mut config = ExecutorConfig {
        default_provider: Arc::new(MockProvider),
        provider_router: None,
        memory: Arc::new(create_test_store().await),
        working_dir: PathBuf::from("/tmp"),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 8,
        max_pipeline_fanout_total: None,
        guards: Vec::new(),
        max_concurrent_llm_calls: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: crate::context::PipelineContext::default(),
        host_context: crate::host_context::PipelineHostContext::default(),
        embedder: None,
        catalog_dir: None,
        sandbox: octos_agent::SandboxConfig::default(),
    };
    config.working_dir = only_dir.path().to_path_buf();
    let executor = PipelineExecutor::new(config);
    let catalog_dir = executor
        .config
        .catalog_dir
        .as_deref()
        .unwrap_or(&executor.config.working_dir);
    assert_eq!(
        catalog_dir,
        only_dir.path(),
        "without catalog_dir the executor must fall back to working_dir \
             (legacy callers, pre-Phase-2-A behaviour)"
    );
}

// ── Gap 4.2 / Blocker 1: node-progress event line MUST stay under the
// 16 KiB harness-event line cap or the reader silently DROPS it ─────────

/// Blocker 1 (RED on 3d5353d5) — a node event with a pathological 4 KiB
/// `node_id` PLUS a 2 KiB all-control-byte preview (which JSON-escapes ~6x
/// to ~12 KiB) serializes to >16 KiB and would be DROPPED by the reader's
/// `MAX_HARNESS_EVENT_LINE_BYTES` gate — defeating the gap (back to opaque).
/// After the fix the assembled event line is provably under the cap.
#[tokio::test]
async fn pathological_node_event_stays_under_line_cap() {
    use octos_agent::harness_events::{
        HarnessEvent, HarnessEventSinkContext, MAX_HARNESS_EVENT_LINE_BYTES,
        attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-pipeline-blocker1".to_string(),
        },
    );

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-pipeline-blocker1".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    // A 4 KiB node_id (free-form, unbounded at the call site) and a 2 KiB
    // body that is ALL NUL bytes — each escapes to ` ` (6 bytes) so a
    // naive 2 KiB preview balloons to ~12 KiB serialized. node_id + preview
    // + a long message together blow past the 16 KiB line cap.
    let long_node_id = "n".repeat(4 * 1024);
    let control_body = "\u{0}".repeat(2 * 1024);
    let preview = node_output_preview(&control_body);
    // A max-allowed message (the validator already caps `message` at 2 KiB):
    // the OVER-CAP comes from the unbounded `node_id` + control-byte preview
    // in `extra`, which the Progress validator never inspects.
    let long_message = format!("{} (2 of 3)", "M".repeat(2000));

    let sink_for_assert = sink_uri.clone();
    TOOL_CTX
        .scope(ctx, async move {
            emit_pipeline_node_event(
                "research",
                "node_completed",
                &long_message,
                &long_node_id,
                2,
                3,
                Some(true),
                Some(&preview),
            );
        })
        .await;

    detach_event_sink_context(&sink_uri);

    let lines = std::fs::read_to_string(&sink_for_assert).expect("read sink");
    let event_lines: Vec<&str> = lines.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        event_lines.len(),
        1,
        "expected exactly one node_completed event line"
    );
    let line = event_lines[0];
    assert!(
        line.len() < MAX_HARNESS_EVENT_LINE_BYTES,
        "node event line must stay under the {MAX_HARNESS_EVENT_LINE_BYTES}-byte cap \
             (else the reader DROPS it); got {} bytes",
        line.len()
    );
    // And it must still be a valid, readable event (not dropped/garbled).
    let event = HarnessEvent::from_json_line(line).expect("event must round-trip (not dropped)");
    let detail = event.runtime_detail_value(None, None);
    assert_eq!(detail["phase"], "node_completed");
    // node_id is bounded but still present (truncation marker is fine).
    let node = detail["node"].as_str().expect("node field present");
    assert!(
        node.len() <= NODE_ID_MAX_CHARS + 16,
        "node_id must be bounded to its cap; got {} bytes",
        node.len()
    );
}

/// Blocker 1 (RED on 27c26433) — the node-event `message` (the
/// `label (N of M)` string) was NOT in the line budget and was only
/// raw-byte-bounded to 2 KiB *downstream*. Two failure modes:
///   1. a control-byte-heavy `message` just under 2 KiB raw escapes ~6× to
///      ~12 KiB, which — added to the ~10 KiB free-form budget for
///      node_id + preview — pushes the serialized line PAST 16 KiB and the
///      reader DROPS it; and
///   2. a `message` *over* 2 KiB raw (a long node label) is REJECTED by the
///      validator (`message exceeded 2048 bytes`) → the event never emits.
/// After the fix the message is bounded by its escaped length (so it never
/// trips the validator) AND counted in the line budget, so the serialized
/// line is provably under the cap and the event always emits.
#[tokio::test]
async fn pathological_node_label_message_stays_under_line_cap() {
    use octos_agent::harness_events::{
        HarnessEvent, HarnessEventSinkContext, MAX_HARNESS_EVENT_LINE_BYTES,
        attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-pipeline-blocker1-label".to_string(),
        },
    );

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-pipeline-blocker1-label".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    // A pathological node LABEL → message: 8 KiB of NUL bytes (each escapes
    // to ` ` = 6 bytes, so raw 8 KiB → ~48 KiB escaped) plus the
    // `(N of M)` suffix the call sites append. This both (a) exceeds the
    // 2 KiB raw `message` validator bound (→ rejected, no emit) AND (b)
    // would balloon the serialized line far past 16 KiB. Combined with a
    // 4 KiB free-form node_id and a 2 KiB all-control-byte preview, an
    // unbounded message guarantees an over-cap (or rejected) line.
    let control_label = "\u{0}".repeat(8 * 1024);
    let long_message = format!("{control_label} (2 of 3)");
    let long_node_id = "n".repeat(4 * 1024);
    let control_body = "\u{0}".repeat(2 * 1024);
    let preview = node_output_preview(&control_body);

    let sink_for_assert = sink_uri.clone();
    TOOL_CTX
        .scope(ctx, async move {
            emit_pipeline_node_event(
                "research",
                "node_completed",
                &long_message,
                &long_node_id,
                2,
                3,
                Some(true),
                Some(&preview),
            );
        })
        .await;

    detach_event_sink_context(&sink_uri);

    let lines = std::fs::read_to_string(&sink_for_assert).expect("read sink");
    let event_lines: Vec<&str> = lines.lines().filter(|l| !l.trim().is_empty()).collect();
    // The event must EMIT (a long/control-byte label must not silently drop
    // the whole event by tripping the 2 KiB raw-message validator bound).
    assert_eq!(
        event_lines.len(),
        1,
        "a pathological node LABEL must still emit exactly one node_completed \
             event (not be rejected by the message validator); got {event_lines:?}"
    );
    let line = event_lines[0];
    assert!(
        line.len() < MAX_HARNESS_EVENT_LINE_BYTES,
        "node event line (incl. message) must stay under the \
             {MAX_HARNESS_EVENT_LINE_BYTES}-byte cap (else the reader DROPS it); \
             got {} bytes",
        line.len()
    );
    let event = HarnessEvent::from_json_line(line).expect("event must round-trip (not dropped)");
    let detail = event.runtime_detail_value(None, None);
    assert_eq!(detail["phase"], "node_completed");
}

// ── Gap 4.2 / Blocker 2: Parallel + DynamicParallel sub-nodes MUST emit
// structured per-node events (deep_research IS dynamic_parallel) ─────────

/// Drain all `node_started`/`node_completed` events written to `sink_path`
/// and return `(node_label, phase, success)` tuples for assertion.
fn drain_node_events(sink_path: &str) -> Vec<(String, String, Option<bool>)> {
    use octos_agent::harness_events::HarnessEvent;
    let lines = std::fs::read_to_string(sink_path).unwrap_or_default();
    lines
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| HarnessEvent::from_json_line(l).ok())
        .map(|e| e.runtime_detail_value(None, None))
        .filter(|d| d["phase"] == "node_started" || d["phase"] == "node_completed")
        .map(|d| {
            (
                d["node"].as_str().unwrap_or_default().to_string(),
                d["phase"].as_str().unwrap_or_default().to_string(),
                d["success"].as_bool(),
            )
        })
        .collect()
}

/// Blocker 2 (RED on 3d5353d5) — a static `parallel` fan-out must emit a
/// structured `node_started` + `node_completed` for EACH sub-node. Before
/// the fix the parallel branch `continue`s before the sequential emit
/// sites, so a parallel pipeline emitted NO per-node structured progress.
#[tokio::test]
async fn parallel_subnodes_emit_structured_events() {
    use octos_agent::harness_events::{
        HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-pipeline-parallel".to_string(),
        },
    );

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-pipeline-parallel".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    let dot = r#"
            digraph t {
                fan [handler="parallel", converge="merge"]
                a [handler="noop"]
                b [handler="noop"]
                merge [handler="noop"]
                fan -> a
                fan -> b
                a -> merge
                b -> merge
            }
        "#;

    let sink_for_run = sink_uri.clone();
    let result = TOOL_CTX
        .scope(ctx, async move {
            let config = make_capped_config(8).await;
            let executor = PipelineExecutor::new(config);
            executor
                .run(dot, "parallel happy path", &serde_json::Map::new())
                .await
        })
        .await;
    assert!(
        result.is_ok(),
        "parallel pipeline should complete: {result:?}"
    );

    detach_event_sink_context(&sink_uri);

    let events = drain_node_events(&sink_for_run);
    for sub in ["a", "b"] {
        assert!(
            events
                .iter()
                .any(|(n, p, _)| n == sub && p == "node_started"),
            "parallel sub-node '{sub}' must emit node_started; got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|(n, p, s)| n == sub && p == "node_completed" && *s == Some(true)),
            "parallel sub-node '{sub}' must emit node_completed(success); got {events:?}"
        );
    }
}

/// Blocker 2 (RED on 3d5353d5) — a `dynamic_parallel` node (the shape
/// `deep_research` uses) must emit structured per-sub-node events for each
/// dynamically-expanded worker. The `MockProvider` planner returns plain
/// "done" → JSON extraction fails → the 3-task fallback expands, so we
/// expect node_started + node_completed for each fallback worker task.
#[tokio::test]
async fn dynamic_parallel_subnodes_emit_structured_events() {
    use octos_agent::harness_events::{
        HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-pipeline-dynparallel".to_string(),
        },
    );

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-pipeline-dynparallel".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    let dot = r#"
            digraph t {
                plan [handler="dynamic_parallel", converge="merge", prompt="plan"]
                merge [handler="noop"]
                plan -> merge
            }
        "#;

    let sink_for_run = sink_uri.clone();
    let result = TOOL_CTX
        .scope(ctx, async move {
            // Generous cap so the 3-task fallback fan-out runs to completion.
            let config = make_capped_config(64).await;
            let executor = PipelineExecutor::new(config);
            executor
                .run(dot, "dynamic happy path", &serde_json::Map::new())
                .await
        })
        .await;
    assert!(
        result.is_ok(),
        "dynamic_parallel pipeline should complete: {result:?}"
    );

    detach_event_sink_context(&sink_uri);

    let events = drain_node_events(&sink_for_run);
    let started = events
        .iter()
        .filter(|(_, p, _)| p == "node_started")
        .count();
    let completed = events
        .iter()
        .filter(|(_, p, _)| p == "node_completed")
        .count();
    assert!(
        started >= 2,
        "dynamic_parallel must emit a node_started per worker (>=2 fallback tasks); got {started} ({events:?})"
    );
    assert_eq!(
        started, completed,
        "every dynamic worker that starts must also emit node_completed; \
             started={started} completed={completed} ({events:?})"
    );
}

/// Blocker 2 (RED on 27c26433) — when a LATER sub-node's fan-out PREP
/// fails (here: a per-contract budget reservation that admits the 1st
/// codergen target but REJECTS the 2nd), the run loop early-returns via `?`
/// BEFORE `join_all`, so any future already pushed is never polled. On
/// 27c26433 `node_started` was emitted in the prep loop (outside the
/// future), so the 1st target's `node_started` was emitted with no matching
/// `node_completed` → a chip stuck "running" forever. After the fix the
/// `node_started` emit lives INSIDE each future, so a future that never runs
/// emits NOTHING and `node_started` count == `node_completed` count.
#[tokio::test]
async fn parallel_prep_failure_leaves_no_dangling_node_started() {
    use octos_agent::cost_ledger::{CostAccountant, CostBudgetPolicy, PersistentCostLedger};
    use octos_agent::harness_events::{
        HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-pipeline-prepfail".to_string(),
        },
    );

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-pipeline-prepfail".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    // Per-contract ceiling sized against the reservation SEQUENCE:
    // pipeline-level (0.001 below) + 1st codergen target (0.001) = 0.002 is
    // ADMITTED, but adding the 2nd target (0.003) trips the 0.0025 ceiling.
    // The 2nd reservation `?`-propagates out of the fan-out prep loop before
    // `join_all`, abandoning the 1st target's already-pushed future.
    let ledger_dir = tempfile::tempdir().expect("ledger dir");
    let ledger = PersistentCostLedger::open(ledger_dir.path())
        .await
        .expect("open cost ledger");
    let policy = CostBudgetPolicy::default().with_per_contract_usd(0.0025);
    let accountant = Arc::new(CostAccountant::new(Arc::new(ledger), Some(policy)));

    // Two codergen fan-out targets (codergen reserves; noop does not).
    let dot = r#"
            digraph t {
                fan [handler="parallel", converge="merge"]
                a [handler="codergen", prompt="a"]
                b [handler="codergen", prompt="b"]
                merge [handler="noop"]
                fan -> a
                fan -> b
                a -> merge
                b -> merge
            }
        "#;

    let sink_for_run = sink_uri.clone();
    let result = TOOL_CTX
        .scope(ctx, async move {
            let mut config = make_capped_config(8).await;
            config.workspace_context = crate::context::PipelineContext::new()
                .with_cost_accountant(accountant)
                // Small pipeline-level projection so the per-NODE fan-out
                // reservations (not the pipeline reserve) are what trips the
                // ceiling on the 2nd target.
                .with_projected_usd(0.001);
            let executor = PipelineExecutor::new(config);
            executor
                .run(dot, "parallel prep failure", &serde_json::Map::new())
                .await
        })
        .await;

    detach_event_sink_context(&sink_uri);

    // The pipeline is EXPECTED to fail (budget breach on the 2nd target).
    assert!(
        result.is_err(),
        "expected the fan-out to fail on the 2nd reservation; got {result:?}"
    );

    let events = drain_node_events(&sink_for_run);
    let started = events
        .iter()
        .filter(|(_, p, _)| p == "node_started")
        .count();
    let completed = events
        .iter()
        .filter(|(_, p, _)| p == "node_completed")
        .count();
    assert_eq!(
        started, completed,
        "every emitted node_started must have a matching node_completed even \
             when fan-out prep aborts early (no stuck-running chip); \
             started={started} completed={completed} ({events:?})"
    );
}

/// Blocker 2 (RED on 27c26433) — same dangling-`node_started` guard for
/// `dynamic_parallel` (the shape `deep_research` uses). A LATER worker's
/// budget reservation is rejected, aborting the fan-out prep loop before
/// `join_all`; no worker may be left with a `node_started` and no matching
/// `node_completed`.
#[tokio::test]
async fn dynamic_parallel_prep_failure_leaves_no_dangling_node_started() {
    use octos_agent::cost_ledger::{CostAccountant, CostBudgetPolicy, PersistentCostLedger};
    use octos_agent::harness_events::{
        HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-pipeline-dp-prepfail".to_string(),
        },
    );

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-pipeline-dp-prepfail".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    // The 3-task fallback expands (MockProvider returns "done" → JSON
    // extraction fails). Reservation sequence: pipeline (0.001) + worker1
    // (0.001) = 0.002 ADMITTED; adding worker2 (0.003) trips the 0.0025
    // ceiling, aborting the prep loop before `join_all`.
    let ledger_dir = tempfile::tempdir().expect("ledger dir");
    let ledger = PersistentCostLedger::open(ledger_dir.path())
        .await
        .expect("open cost ledger");
    let policy = CostBudgetPolicy::default().with_per_contract_usd(0.0025);
    let accountant = Arc::new(CostAccountant::new(Arc::new(ledger), Some(policy)));

    let dot = r#"
            digraph t {
                plan [handler="dynamic_parallel", converge="merge", prompt="plan"]
                merge [handler="noop"]
                plan -> merge
            }
        "#;

    let sink_for_run = sink_uri.clone();
    let result = TOOL_CTX
        .scope(ctx, async move {
            let mut config = make_capped_config(64).await;
            config.workspace_context = crate::context::PipelineContext::new()
                .with_cost_accountant(accountant)
                .with_projected_usd(0.001);
            let executor = PipelineExecutor::new(config);
            executor
                .run(dot, "dynamic prep failure", &serde_json::Map::new())
                .await
        })
        .await;

    detach_event_sink_context(&sink_uri);

    assert!(
        result.is_err(),
        "expected the dynamic fan-out to fail on a later reservation; got {result:?}"
    );

    let events = drain_node_events(&sink_for_run);
    let started = events
        .iter()
        .filter(|(_, p, _)| p == "node_started")
        .count();
    let completed = events
        .iter()
        .filter(|(_, p, _)| p == "node_completed")
        .count();
    assert_eq!(
        started, completed,
        "dynamic_parallel: every emitted node_started must have a matching \
             node_completed even when prep aborts early; \
             started={started} completed={completed} ({events:?})"
    );
}

/// NIT (RED on 3d5353d5 if multiplication were unguarded) — the linear ETA
/// must SATURATE instead of overflowing when `per_node * remaining` would
/// exceed `u64::MAX`. A pathological huge `elapsed` with many nodes
/// remaining must not panic / wrap.
#[test]
fn linear_eta_saturates_on_huge_elapsed() {
    // per_node = u64::MAX / 1 = u64::MAX; remaining = large → would overflow
    // a plain `*`. Must clamp to u64::MAX, not wrap.
    let eta = linear_eta_secs(u64::MAX, 1, 1_000_000);
    assert_eq!(eta, Some(u64::MAX), "ETA must saturate, not overflow");
}

// ── Gap 4.2 / Blocker 3: an unbounded graph/workflow id must not silently
// DROP the whole node event (workflow > 128 B fails the validator) ───────

/// Blocker 3 (RED on cab744a4) — `emit_pipeline_node_event` copies the
/// graph id verbatim into the event `workflow`, but the DOT parser accepts
/// an UNBOUNDED graph id and the harness validator REJECTS `workflow >128 B`.
/// A pathological >128-byte graph id therefore makes `write_event_to_sink`
/// reject the event — and the preview-shrink loop can't fix it (the id is
/// not elastic) — so the event silently DROPS (back to opaque). After the
/// fix the workflow id is truncated at the emit site to the validator limit,
/// so the line is provably emittable with preview shrunk all the way to 0.
#[tokio::test]
async fn oversized_graph_id_node_event_still_emits() {
    use octos_agent::harness_events::{
        HarnessEvent, HarnessEventSinkContext, MAX_HARNESS_EVENT_LINE_BYTES,
        attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-pipeline-blocker3".to_string(),
        },
    );

    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-pipeline-blocker3".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    // A 512-byte graph id (well over the 128-byte MAX_WORKFLOW_BYTES) plus a
    // big preview. The id is NOT elastic, so without an emit-site bound the
    // validator rejects the event and nothing is written.
    let huge_graph_id = "g".repeat(512);
    let preview = node_output_preview(&"z".repeat(50_000));

    let sink_for_assert = sink_uri.clone();
    let huge_for_assert = huge_graph_id.clone();
    TOOL_CTX
        .scope(ctx, async move {
            emit_pipeline_node_event(
                &huge_graph_id,
                "node_started",
                "analyze (1 of 2)",
                "analyze",
                1,
                2,
                None,
                Some(&preview),
            );
        })
        .await;

    detach_event_sink_context(&sink_uri);

    let lines = std::fs::read_to_string(&sink_for_assert).expect("read sink");
    let event_lines: Vec<&str> = lines.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        event_lines.len(),
        1,
        "an oversized graph id must NOT drop the event; expected exactly one \
             node event line, got {event_lines:?}"
    );
    let line = event_lines[0];
    assert!(
        line.len() < MAX_HARNESS_EVENT_LINE_BYTES,
        "node event line must stay under the {MAX_HARNESS_EVENT_LINE_BYTES}-byte cap; \
             got {} bytes",
        line.len()
    );
    let event = HarnessEvent::from_json_line(line).expect("event must round-trip (not dropped)");
    let detail = event.runtime_detail_value(None, None);
    assert_eq!(detail["phase"], "node_started");
    // The workflow id was truncated to the validator bound — prefix preserved.
    let workflow = detail["workflow_kind"].as_str().expect("workflow present");
    assert!(
        workflow.len() <= 128,
        "workflow id must be truncated to the validator bound; got {} bytes",
        workflow.len()
    );
    assert!(
        huge_for_assert.starts_with(workflow) || workflow.starts_with("gg"),
        "truncated workflow must be a prefix of the original id; got {workflow:?}"
    );
}

// ── Gap 4.2 / Blocker 1+2: NodeProgressGuard — every node_started gets a
// matching node_completed on EVERY exit path (error, panic, cancellation) ─

/// A test handler whose `execute` returns `Err` on the first call — drives
/// the SEQUENTIAL dispatch `?`-early-return path between the `node_started`
/// and the (skipped) `node_completed` emit.
struct ErroringHandler;
#[async_trait::async_trait]
impl crate::handler::Handler for ErroringHandler {
    async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
        eyre::bail!("handler '{}' hard-errored on purpose", node.id)
    }
}

/// A test handler whose `execute` PANICS — exercises the guard's Drop on
/// unwind (a panic between node_started and node_completed must still flip
/// the chip off "running").
struct PanickingHandler;
#[async_trait::async_trait]
impl crate::handler::Handler for PanickingHandler {
    async fn execute(&self, _node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
        panic!("handler panicked on purpose");
    }
}

/// A test handler that NEVER returns (parks forever) so the run future can
/// be polled once into the node, then dropped (cancellation) mid-node.
struct HangingHandler;
#[async_trait::async_trait]
impl crate::handler::Handler for HangingHandler {
    async fn execute(&self, _node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
        std::future::pending::<()>().await;
        unreachable!()
    }
}

fn install_handler(
    kind: HandlerKind,
    handler: Arc<dyn crate::handler::Handler>,
) -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    registry.register(kind, handler);
    // DynamicParallel needs a (noop) registry entry even when unused.
    registry.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));
    registry.register(HandlerKind::Noop, Arc::new(NoopHandler));
    registry
}

/// Blocker 1 (RED on cab744a4) — a SEQUENTIAL node whose dispatch errors
/// (`?`-returns out of the loop between the node_started emit and the
/// node_completed emit) must STILL get a matching `node_completed{success:
/// false}` via the RAII guard's Drop — otherwise the chip is stuck "running".
#[tokio::test]
async fn sequential_dispatch_error_emits_node_completed_via_guard() {
    use octos_agent::harness_events::{
        HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-seq-error".to_string(),
        },
    );
    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-seq-error".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    // Single codergen node whose handler hard-errors → dispatch `?`-returns.
    let dot = r#"
            digraph t {
                solo [handler="codergen", prompt="go"]
            }
        "#;

    let sink_for_run = sink_uri.clone();
    let result = TOOL_CTX
        .scope(ctx, async move {
            let config = make_capped_config(8).await;
            let executor = PipelineExecutor::new(config);
            let handlers = install_handler(HandlerKind::Codergen, Arc::new(ErroringHandler));
            executor
                .run_with_handlers(dot, "seq error", &serde_json::Map::new(), handlers)
                .await
        })
        .await;

    detach_event_sink_context(&sink_uri);
    assert!(
        result.is_err(),
        "erroring dispatch must surface as Err: {result:?}"
    );

    let events = drain_node_events(&sink_for_run);
    let started = events
        .iter()
        .filter(|(_, p, _)| p == "node_started")
        .count();
    let completed = events
        .iter()
        .filter(|(_, p, _)| p == "node_completed")
        .count();
    assert_eq!(
        (started, completed),
        (1, 1),
        "a sequential node that errors must emit exactly one node_started AND \
             one node_completed (no dangling); got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|(n, p, s)| n == "solo" && p == "node_completed" && *s == Some(false)),
        "the guard-Drop completion must mark the interrupted node failed; got {events:?}"
    );
}

/// Blocker 1 (RED on cab744a4) — a node whose handler PANICS must still emit
/// `node_completed` via the guard's Drop during unwind. We catch the panic
/// at the run boundary so the test observes the emitted events.
#[tokio::test]
async fn sequential_panic_emits_node_completed_via_guard_drop() {
    use octos_agent::harness_events::{
        HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-seq-panic".to_string(),
        },
    );
    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-seq-panic".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    let dot = r#"
            digraph t {
                solo [handler="codergen", prompt="go"]
            }
        "#;

    let sink_for_run = sink_uri.clone();
    // Run on a separate tokio task so the panic is contained and joined,
    // letting the guard's Drop (synchronous emit) run during unwind.
    let join = tokio::spawn(async move {
        TOOL_CTX
            .scope(ctx, async move {
                let config = make_capped_config(8).await;
                let executor = PipelineExecutor::new(config);
                let handlers = install_handler(HandlerKind::Codergen, Arc::new(PanickingHandler));
                executor
                    .run_with_handlers(dot, "seq panic", &serde_json::Map::new(), handlers)
                    .await
            })
            .await
    })
    .await;

    detach_event_sink_context(&sink_uri);
    assert!(
        join.is_err(),
        "the handler panic must propagate as a join error"
    );

    let events = drain_node_events(&sink_for_run);
    let started = events
        .iter()
        .filter(|(_, p, _)| p == "node_started")
        .count();
    let completed = events
        .iter()
        .filter(|(_, p, _)| p == "node_completed")
        .count();
    assert_eq!(
        (started, completed),
        (1, 1),
        "a panicking node must still emit node_completed via guard Drop; got {events:?}"
    );
}

/// Blocker 1 (RED on cab744a4) — a CANCELLED run (the run future is dropped
/// mid-node) must flip every started node off "running" via the guard's
/// Drop. The guard captures the sink at arm time, so Drop works even though
/// the TOOL_CTX task-local is gone when the future is dropped.
#[tokio::test]
async fn cancelled_run_emits_node_completed_via_guard_drop() {
    use octos_agent::harness_events::{
        HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-cancel".to_string(),
        },
    );
    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-cancel".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    let dot = r#"
            digraph t {
                solo [handler="codergen", prompt="go"]
            }
        "#;

    let sink_for_run = sink_uri.clone();
    TOOL_CTX
        .scope(ctx, async move {
            let config = make_capped_config(8).await;
            let executor = PipelineExecutor::new(config);
            let handlers = install_handler(HandlerKind::Codergen, Arc::new(HangingHandler));
            let vars = serde_json::Map::new();
            let run = executor.run_with_handlers(dot, "cancel", &vars, handlers);
            // Drive the run far enough to enter the node (emit node_started +
            // park on the hanging handler), then DROP it (cancellation).
            let timed = tokio::time::timeout(Duration::from_millis(150), run).await;
            assert!(timed.is_err(), "the hanging handler must not complete");
            // `timed` (and the inner run future) is dropped here → guard Drop.
        })
        .await;

    detach_event_sink_context(&sink_uri);

    let events = drain_node_events(&sink_for_run);
    let started = events
        .iter()
        .filter(|(_, p, _)| p == "node_started")
        .count();
    let completed = events
        .iter()
        .filter(|(_, p, _)| p == "node_completed")
        .count();
    assert_eq!(
        started, completed,
        "a cancelled run must complete every started node via guard Drop; \
             started={started} completed={completed} ({events:?})"
    );
    assert!(
        started >= 1,
        "the run must have entered the node; got {events:?}"
    );
}

/// Guard happy-path regression — a SEQUENTIAL node that completes normally
/// must emit EXACTLY one `node_started` and EXACTLY one `node_completed`
/// (success): `complete()` disarms the guard so its Drop does NOT fire a
/// second terminal event. Locks the "exactly one started + one completed per
/// node that runs; no double-emit" invariant for the sequential path.
#[tokio::test]
async fn sequential_happy_path_emits_exactly_one_pair() {
    use octos_agent::harness_events::{
        HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-seq-happy".to_string(),
        },
    );
    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-seq-happy".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    // Single noop node — completes normally; the guard must disarm.
    let dot = r#"
            digraph t {
                solo [handler="noop"]
            }
        "#;

    let sink_for_run = sink_uri.clone();
    let result = TOOL_CTX
        .scope(ctx, async move {
            let config = make_capped_config(8).await;
            let executor = PipelineExecutor::new(config);
            executor
                .run(dot, "seq happy", &serde_json::Map::new())
                .await
        })
        .await;

    detach_event_sink_context(&sink_uri);
    assert!(
        result.is_ok(),
        "happy-path sequential run must succeed: {result:?}"
    );

    let events = drain_node_events(&sink_for_run);
    let started: Vec<_> = events
        .iter()
        .filter(|(_, p, _)| p == "node_started")
        .collect();
    let completed: Vec<_> = events
        .iter()
        .filter(|(_, p, _)| p == "node_completed")
        .collect();
    assert_eq!(
        (started.len(), completed.len()),
        (1, 1),
        "a sequential node that completes normally must emit EXACTLY one \
             started + one completed (guard disarmed, no double-emit); got {events:?}"
    );
    assert_eq!(
        completed[0].2,
        Some(true),
        "the normal completion must report success=true, not the guard's \
             interrupted fallback; got {events:?}"
    );
}

/// Blocker 2 (RED on cab744a4) — a PANIC inside a Parallel sub-node future
/// must still emit `node_completed` for that sub-node via the guard's Drop
/// (the future unwinds; `join_all` surfaces the panic). Without the guard,
/// the panicking worker's node_started dangles.
#[tokio::test]
async fn parallel_subnode_panic_emits_node_completed_via_guard_drop() {
    use octos_agent::harness_events::{
        HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
    };

    let sink_file = tempfile::NamedTempFile::new().expect("sink file");
    let sink_uri = sink_file.path().display().to_string();
    attach_event_sink_context(
        sink_uri.clone(),
        HarnessEventSinkContext {
            session_id: "api:session".to_string(),
            task_id: "tc-par-panic".to_string(),
        },
    );
    let ctx = octos_agent::tools::ToolContext {
        tool_id: "tc-par-panic".to_string(),
        harness_event_sink: Some(sink_uri.clone()),
        ..octos_agent::tools::ToolContext::zero()
    };

    // Parallel fan-out where each sub-node is a codergen target whose
    // handler panics. `join_all` polls the worker futures on THIS task.
    let dot = r#"
            digraph t {
                fan [handler="parallel", converge="merge"]
                a [handler="codergen", prompt="a"]
                merge [handler="noop"]
                fan -> a
                a -> merge
            }
        "#;

    let sink_for_run = sink_uri.clone();
    let join = tokio::spawn(async move {
        TOOL_CTX
            .scope(ctx, async move {
                let config = make_capped_config(8).await;
                let executor = PipelineExecutor::new(config);
                let handlers = install_handler(HandlerKind::Codergen, Arc::new(PanickingHandler));
                executor
                    .run_with_handlers(dot, "par panic", &serde_json::Map::new(), handlers)
                    .await
            })
            .await
    })
    .await;

    detach_event_sink_context(&sink_uri);
    // The worker panic propagates through join_all → the run task panics.
    assert!(join.is_err(), "a panicking parallel worker must propagate");

    let events = drain_node_events(&sink_for_run);
    let started = events
        .iter()
        .filter(|(_, p, _)| p == "node_started")
        .count();
    let completed = events
        .iter()
        .filter(|(_, p, _)| p == "node_completed")
        .count();
    assert_eq!(
        started, completed,
        "every parallel sub-node that starts must emit node_completed even on \
             panic; started={started} completed={completed} ({events:?})"
    );
    assert!(
        started >= 1,
        "the parallel worker must have started; got {events:?}"
    );
}

/// A pipeline node whose model key is NOT registered in the provider router
/// must degrade to the default provider rather than kill the run (#1901).
///
/// `model_assignment` rewrites DOT lane keys (`model="strong"`) to concrete
/// catalog models, and a profile without a filtered `pipeline_models.json`
/// falls back to the UNFILTERED catalog — so it can name models the router
/// never registered. `resolve_provider` propagated that with `?`, failing the
/// whole pipeline, while `CodergenHandler::resolve_provider` (handler.rs) has
/// always degraded in the same situation. The rotating model name in the error
/// (`qwen3-max`, then `qwen-plus`, …) came from `ModelPools::nth_strong`
/// round-robining the catalog, which is what made it look intermittent.
///
/// Mirrors handler.rs's `resolve_provider_falls_back_to_coding_llm_when_key_absent`
/// so the two paths cannot drift apart again.
#[tokio::test]
async fn resolve_provider_falls_back_to_default_when_key_absent() {
    struct NamedMock(&'static str);

    #[async_trait::async_trait]
    impl LlmProvider for NamedMock {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            unreachable!("resolve_provider must not call chat()")
        }
        fn model_id(&self) -> &str {
            self.0
        }
        fn provider_name(&self) -> &str {
            self.0
        }
    }

    let default: Arc<dyn LlmProvider> = Arc::new(NamedMock("pipeline-default"));
    let router = Arc::new(ProviderRouter::new());
    router.register("strong", Arc::new(NamedMock("research-strong")));

    // The bug: an assigned-but-unregistered model must NOT fail the run.
    let fallback = resolve_provider(&default, Some(&router), Some("qwen3-max"))
        .expect("an unresolvable model key must not error the pipeline");
    assert_eq!(
        fallback.model_id(),
        "pipeline-default",
        "an unregistered model key must degrade to the default provider"
    );

    // A registered key still resolves to its own lane — the fallback must not
    // swallow everything.
    let resolved = resolve_provider(&default, Some(&router), Some("strong"))
        .expect("a registered key must resolve");
    assert_eq!(
        resolved.model_id(),
        "research-strong",
        "a resolvable key must use its lane, not the default"
    );

    // No key at all: unchanged behaviour.
    let none = resolve_provider(&default, Some(&router), None).expect("no key must resolve");
    assert_eq!(none.model_id(), "pipeline-default");
}
