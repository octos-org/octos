//! M8.7 spawn/runtime wiring tests (item 4 of fix-first checklist).
//!
//! These tests pin the production wiring of `SubAgentOutputRouter` and
//! `AgentSummaryGenerator` into the real agent execution path:
//!
//! - The watcher is spawned the moment a spawn_only background task
//!   begins (after the `min_runtime` warm-up).
//! - The watcher is stopped and the router's task is marked terminal
//!   the moment the supervisor records a terminal status.
//! - `AgentSummaryGenerator::min_runtime` is now actually consulted —
//!   short tasks that finish inside the warm-up never trigger a tick.
//! - Background tasks update `runtime_detail` from the real watcher,
//!   not only from one-shot manual `summarize_once` calls.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::{
    Agent, AgentConfig, AgentSummaryGenerator, SubAgentOutputRouter, TaskStatus, TaskSupervisor,
    Tool, ToolRegistry, ToolResult, subagent_summary::DEFAULT_SUBAGENT_SUMMARY_TICK,
};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

/// Bookkeeping for a fake spawn_only background tool — captures how
/// long it slept so the test can reason about whether the watcher's
/// `min_runtime` blocked the first tick.
struct SleepyTool {
    name: &'static str,
    sleep: Duration,
    invocations: Arc<AtomicU32>,
}

impl SleepyTool {
    fn new(name: &'static str, sleep: Duration) -> Self {
        Self {
            name,
            sleep,
            invocations: Arc::new(AtomicU32::new(0)),
        }
    }
}

#[async_trait]
impl Tool for SleepyTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "fake spawn_only worker"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.sleep).await;
        Ok(ToolResult {
            output: format!("{} done", self.name),
            success: true,
            ..Default::default()
        })
    }
}

struct MockLlm {
    responses: Mutex<Vec<ChatResponse>>,
}

impl MockLlm {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            eyre::bail!("MockLlm: no more scripted responses");
        }
        Ok(responses.remove(0))
    }
    fn context_window(&self) -> u32 {
        128_000
    }
    fn model_id(&self) -> &str {
        "mock-m87"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Mock LLM whose `chat()` response is deterministic — used as the
/// summary-generator's cheap-lane provider so tests don't fly off to a
/// real network.
struct CheapMock {
    body: String,
    calls: Arc<AtomicU32>,
}

impl CheapMock {
    fn new(body: &str) -> (Arc<Self>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        (
            Arc::new(Self {
                body: body.into(),
                calls: calls.clone(),
            }),
            calls,
        )
    }
}

#[async_trait]
impl LlmProvider for CheapMock {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            content: Some(self.body.clone()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            provider_index: None,
        })
    }
    fn context_window(&self) -> u32 {
        128_000
    }
    fn model_id(&self) -> &str {
        "mock-cheap"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn tool_use(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls: calls,
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 50,
            output_tokens: 5,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn end(text: &str) -> ChatResponse {
    ChatResponse {
        content: Some(text.into()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 5,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn tc(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: serde_json::json!({}),
        metadata: None,
    }
}

#[tokio::test]
async fn spawn_only_runtime_writes_output_to_router_in_production_path() {
    // Wire a SubAgentOutputRouter into the agent. After running a
    // spawn_only tool through the production path, the router must have
    // been told the task is terminal — this proves the wiring is
    // present in execution.rs (not only in unit tests).
    let _dir = TempDir::new().unwrap();
    let memory_dir = TempDir::new().unwrap();
    let output_root = TempDir::new().unwrap();

    let router = Arc::new(SubAgentOutputRouter::new(output_root.path()));
    let probe = SleepyTool::new("bg_router_probe", Duration::from_millis(20));

    let mut tools = ToolRegistry::new();
    tools.register(probe);
    tools.mark_spawn_only("bg_router_probe", None);
    let supervisor = tools.supervisor();
    let memory = Arc::new(
        EpisodeStore::open(memory_dir.path().join(".octos"))
            .await
            .unwrap(),
    );
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlm::new(vec![
        tool_use(vec![tc("call_router", "bg_router_probe")]),
        end("done"),
    ]));

    let agent = Agent::new(AgentId::new("m87-router"), llm, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        })
        .with_subagent_output_router(router.clone());

    let _ = agent
        .process_message("kick router-probed spawn-only", &[], vec![])
        .await
        .expect("agent loop must succeed");

    // Wait for the bg task to complete + the supervisor to flip status.
    let mut task_id_with_terminal: Option<String> = None;
    for _ in 0..50 {
        for task in supervisor.get_all_tasks() {
            if matches!(task.status, TaskStatus::Completed | TaskStatus::Failed)
                && task.tool_name == "bg_router_probe"
            {
                task_id_with_terminal = Some(task.id.clone());
                break;
            }
        }
        if task_id_with_terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let task_id = task_id_with_terminal.expect("spawn_only task must reach terminal status");
    // Give the spawn_only branch a couple of yields to call mark_terminal
    // after it observed the supervisor's terminal flip.
    for _ in 0..20 {
        if router.is_terminal(&task_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        router.is_terminal(&task_id),
        "router must be told the task is terminal — production-path wiring missing"
    );
    assert_eq!(router.root(), output_root.path());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn spawn_only_runtime_starts_summary_watcher_after_min_runtime() {
    // Build a generator with min_runtime=200ms and tick=50ms. Drive a
    // spawn_only task that finishes BEFORE the warm-up window — the
    // cheap-lane LLM must see zero calls. Then drive a long-running
    // task and assert >= 1 call.
    let supervisor = TaskSupervisor::new();
    let (cheap_provider, llm_calls) = CheapMock::new("doing stuff");

    let activity_buf = std::sync::Arc::new(std::sync::Mutex::new(vec!["line".to_string()]));
    let activity = octos_agent::subagent_summary::ActivitySource::Fixed(activity_buf);
    let generator = AgentSummaryGenerator::with_activity_source(
        cheap_provider,
        Arc::new(activity),
        supervisor.clone(),
    )
    .with_tick(Duration::from_millis(50))
    .with_min_runtime(Duration::from_millis(200))
    .with_llm_timeout(Duration::from_secs(1));

    // Case A: short task finishes before warm-up — zero LLM calls.
    let short_id = supervisor.register("short_task", "call-1", None);
    supervisor.mark_running(&short_id);
    generator.spawn_watcher("api:short", short_id.as_str());
    // Advance only 100ms (still inside warm-up) and complete.
    for _ in 0..2 {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(50)).await;
    }
    supervisor.mark_completed(&short_id, vec![]);
    for _ in 0..6 {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(50)).await;
    }
    assert_eq!(
        llm_calls.load(Ordering::SeqCst),
        0,
        "min_runtime must block the first summary tick for short tasks"
    );

    // Case B: long task that crosses the warm-up window — at least one
    // tick must fire.
    let long_id = supervisor.register("long_task", "call-2", None);
    supervisor.mark_running(&long_id);
    generator.spawn_watcher("api:long", long_id.as_str());
    for _ in 0..10 {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(60)).await;
    }
    supervisor.mark_completed(&long_id, vec![]);
    for _ in 0..4 {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(60)).await;
    }
    assert!(
        llm_calls.load(Ordering::SeqCst) >= 1,
        "long-running task crossing min_runtime must trigger at least one tick: \
         observed={}",
        llm_calls.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn spawn_only_runtime_stops_summary_watcher_and_marks_terminal() {
    // Production path: wire both router + generator; run a spawn_only
    // tool. After completion, the router reports terminal AND the
    // generator's registry no longer contains a watcher for the task.
    let _dir = TempDir::new().unwrap();
    let memory_dir = TempDir::new().unwrap();
    let output_root = TempDir::new().unwrap();

    let router = Arc::new(SubAgentOutputRouter::new(output_root.path()));
    let probe = SleepyTool::new("bg_terminal_probe", Duration::from_millis(20));
    let mut tools = ToolRegistry::new();
    tools.register(probe);
    tools.mark_spawn_only("bg_terminal_probe", None);
    let supervisor = tools.supervisor();
    let memory = Arc::new(
        EpisodeStore::open(memory_dir.path().join(".octos"))
            .await
            .unwrap(),
    );

    let activity_buf = std::sync::Arc::new(std::sync::Mutex::new(vec!["x".to_string()]));
    let activity = octos_agent::subagent_summary::ActivitySource::Fixed(activity_buf);
    let (cheap, _) = CheapMock::new("ok");
    let generator = Arc::new(
        AgentSummaryGenerator::with_activity_source(
            cheap,
            Arc::new(activity),
            (*supervisor).clone(),
        )
        .with_tick(Duration::from_millis(50))
        .with_min_runtime(Duration::from_millis(0))
        .with_llm_timeout(Duration::from_secs(1)),
    );
    let registry = generator.registry();

    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlm::new(vec![
        tool_use(vec![tc("call_terminal", "bg_terminal_probe")]),
        end("done"),
    ]));

    let agent = Agent::new(AgentId::new("m87-terminal"), llm, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        })
        .with_subagent_output_router(router.clone())
        .with_subagent_summary_generator(generator.clone());

    let _ = agent
        .process_message("run terminal-probe", &[], vec![])
        .await
        .expect("agent loop must succeed");

    // Wait for terminal supervisor status.
    let mut task_id_with_terminal: Option<String> = None;
    for _ in 0..50 {
        for task in supervisor.get_all_tasks() {
            if matches!(task.status, TaskStatus::Completed | TaskStatus::Failed)
                && task.tool_name == "bg_terminal_probe"
            {
                task_id_with_terminal = Some(task.id.clone());
                break;
            }
        }
        if task_id_with_terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let task_id = task_id_with_terminal.expect("task must reach terminal");

    // Allow the spawn_only branch to call mark_terminal + stop_watcher.
    for _ in 0..30 {
        if router.is_terminal(&task_id) && !registry.is_active(&task_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        router.is_terminal(&task_id),
        "router not flagged terminal — wiring missing"
    );
    assert!(
        !registry.is_active(&task_id),
        "watcher registry still has task — stop_watcher not called"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn task_runtime_detail_updates_from_real_watcher_not_only_manual_summarize_once() {
    // The watcher loop must call summarize_tick which calls
    // supervisor.apply_harness_event -> mark_runtime_state. Verify the
    // detail string is populated by the periodic loop, NOT only by an
    // explicit summarize_once call. We pin this by NEVER calling
    // summarize_once and asserting the detail still updates.
    let supervisor = TaskSupervisor::new();
    let (cheap, _) = CheapMock::new("running smoothly");
    let activity_buf =
        std::sync::Arc::new(std::sync::Mutex::new(vec!["doing the thing".to_string()]));
    let activity = octos_agent::subagent_summary::ActivitySource::Fixed(activity_buf);
    let generator =
        AgentSummaryGenerator::with_activity_source(cheap, Arc::new(activity), supervisor.clone())
            .with_tick(Duration::from_millis(50))
            .with_min_runtime(Duration::from_millis(0))
            .with_llm_timeout(Duration::from_secs(1));

    let id = supervisor.register("auto_detail", "call-detail", None);
    supervisor.mark_running(&id);
    generator.spawn_watcher("api:detail", id.as_str());

    for _ in 0..10 {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(60)).await;
    }
    let task = supervisor.get_task(&id).expect("task exists");
    assert!(
        task.runtime_detail.is_some(),
        "runtime_detail must be set by the watcher loop without a manual summarize_once"
    );
    // Tick cadence used `DEFAULT_SUBAGENT_SUMMARY_TICK`-independent
    // values here; ensure the test hasn't accidentally degraded to
    // depending on the production default.
    assert_ne!(
        DEFAULT_SUBAGENT_SUMMARY_TICK,
        Duration::from_millis(50),
        "test fixture must override the production tick cadence"
    );

    supervisor.mark_completed(&id, vec![]);
    for _ in 0..4 {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(60)).await;
    }
}
