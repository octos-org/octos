//! Regression: a `turn/interrupt` (or any `supervisor.cancel(task_id)`) must
//! actually ABORT the still-running spawn_only background task — not merely
//! flip its dashboard status while the detached `tokio::spawn` body keeps
//! executing to completion.
//!
//! Origin: the user-reported "pressing Esc / `/stop` does not break a running
//! turn" bug. The serve `turn/interrupt` handler aborts the foreground agent
//! loop, but a `spawn_only` tool (`run_pipeline` / `deep_research`) detaches
//! into its OWN `tokio::spawn` task whose `JoinHandle` is dropped, not awaited.
//! Aborting the agent loop never touched it, and the background body never
//! polled the supervisor's per-task cancel token — so a hung `deep_research`
//! pipeline kept running for minutes after the user asked to stop.
//!
//! Contract pinned here: when the supervisor cancels a running spawn_only
//! task, the detached worker's tool future is DROPPED at the next safe point
//! and the task reaches the terminal `Cancelled` state PROMPTLY — the
//! long-running tool body must NOT run to completion.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, Tool, ToolRegistry, ToolResult};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

struct ScriptedLlm {
    responses: std::sync::Mutex<Vec<ChatResponse>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut r = self.responses.lock().unwrap();
        if r.is_empty() {
            eyre::bail!("ScriptedLlm: no more responses");
        }
        Ok(r.remove(0))
    }
    fn context_window(&self) -> u32 {
        128_000
    }
    fn model_id(&self) -> &str {
        "interrupt-cancel-test"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Flips `dropped_while_running` if the future it lives in is dropped before
/// the body marks itself complete. This is the load-bearing signal: a real
/// cooperative abort DROPS the tool future mid-`await`; a status-only "cancel"
/// that leaves the detached `tokio::spawn` body alive never drops it.
struct AbortSentinel {
    dropped_while_running: Arc<AtomicBool>,
    finished: bool,
}

impl Drop for AbortSentinel {
    fn drop(&mut self) {
        if !self.finished {
            self.dropped_while_running.store(true, Ordering::SeqCst);
        }
    }
}

/// A spawn_only tool whose `execute` blocks for a long time (simulating a hung
/// `deep_research` pipeline). It sets `entered` once it starts and `completed`
/// only if it runs all the way to the end without being aborted. The
/// `AbortSentinel` independently records whether the future was DROPPED
/// mid-flight (the real-abort signal) vs. merely left running.
struct HangingTool {
    name: &'static str,
    entered: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
    dropped_while_running: Arc<AtomicBool>,
    block_for: Duration,
}

#[async_trait]
impl Tool for HangingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "spawn_only hang probe"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        let mut sentinel = AbortSentinel {
            dropped_while_running: self.dropped_while_running.clone(),
            finished: false,
        };
        self.entered.store(true, Ordering::SeqCst);
        // Simulate a long-running pipeline (deep_research fan-out). A
        // cooperative cancel must DROP this future before it finishes.
        tokio::time::sleep(self.block_for).await;
        sentinel.finished = true;
        self.completed.store(true, Ordering::SeqCst);
        Ok(ToolResult {
            output: format!("{} ran to completion\n", self.name),
            success: true,
            ..Default::default()
        })
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

fn end_turn(text: &str) -> ChatResponse {
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

async fn open_memory(dir: &TempDir) -> Arc<EpisodeStore> {
    Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap())
}

struct SpawnOnlyHarness {
    _memory_dir: TempDir,
    _router_dir: Option<TempDir>,
    agent: Agent,
    router: Option<Arc<octos_agent::SubAgentOutputRouter>>,
    entered: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
    dropped_while_running: Arc<AtomicBool>,
    captured_id: Arc<std::sync::Mutex<Option<String>>>,
}

impl SpawnOnlyHarness {
    async fn new(agent_id: &str, routed: bool) -> Self {
        let memory_dir = TempDir::new().unwrap();
        let (router_dir, router) = if routed {
            let dir = TempDir::new().unwrap();
            let router = Arc::new(octos_agent::SubAgentOutputRouter::new(dir.path()));
            (Some(dir), Some(router))
        } else {
            (None, None)
        };
        let entered = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let dropped_while_running = Arc::new(AtomicBool::new(false));
        let probe = HangingTool {
            name: "hang_bg",
            entered: entered.clone(),
            completed: completed.clone(),
            dropped_while_running: dropped_while_running.clone(),
            // Long enough that cancellation must interrupt the body, not wait
            // for natural completion inside the bounded assertions below.
            block_for: Duration::from_secs(30),
        };
        let mut tools = ToolRegistry::new();
        tools.register(probe);
        tools.mark_spawn_only("hang_bg", None);
        let memory = open_memory(&memory_dir).await;
        let llm: Arc<dyn LlmProvider> = Arc::new(ScriptedLlm::new(vec![
            tool_use(vec![tc("call-hang", "hang_bg")]),
            end_turn("kicked off the background pipeline"),
        ]));
        let mut agent =
            Agent::new(AgentId::new(agent_id), llm, tools, memory).with_config(AgentConfig {
                save_episodes: false,
                suppress_auto_send_files: true,
                ..Default::default()
            });
        if let Some(router) = &router {
            agent = agent.with_subagent_output_router(router.clone());
        }
        let captured_id: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let captured = captured_id.clone();
        agent
            .tool_registry()
            .supervisor()
            .set_on_change(move |task| {
                if task.tool_name == "hang_bg" {
                    *captured.lock().unwrap() = Some(task.id.clone());
                }
            });
        Self {
            _memory_dir: memory_dir,
            _router_dir: router_dir,
            agent,
            router,
            entered,
            completed,
            dropped_while_running,
            captured_id,
        }
    }

    async fn launch(&self) -> String {
        let _ = self
            .agent
            .process_message("kick the hanging spawn_only pipeline", &[], vec![])
            .await
            .expect("agent loop should not error launching a spawn_only tool");
        let mut waited = Duration::ZERO;
        while !self.entered.load(Ordering::SeqCst) && waited < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += Duration::from_millis(10);
        }
        assert!(
            self.entered.load(Ordering::SeqCst),
            "background spawn_only tool body never started"
        );
        self.captured_id
            .lock()
            .unwrap()
            .clone()
            .expect("supervisor must have registered the spawn_only task id")
    }

    async fn cancel_and_wait(&self, task_id: &str) {
        let supervisor = self.agent.tool_registry().supervisor();
        supervisor
            .cancel(task_id)
            .expect("cancel of a running task must succeed");
        let mut waited = Duration::ZERO;
        let deadline = Duration::from_secs(3);
        loop {
            let task = supervisor.get_task(task_id);
            let is_cancelled = task
                .as_ref()
                .map(|t| matches!(t.status, octos_agent::TaskStatus::Cancelled))
                .unwrap_or(false);
            if is_cancelled {
                break;
            }
            assert!(
                waited < deadline,
                "task did not reach terminal Cancelled within {deadline:?}; status={:?}",
                task.map(|t| t.status)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += Duration::from_millis(20);
        }
    }
}

#[tokio::test]
async fn supervisor_cancel_aborts_running_spawn_only_task() {
    let harness = SpawnOnlyHarness::new("interrupt-cancel", false).await;
    let task_id = harness.launch().await;
    harness.cancel_and_wait(&task_id).await;

    // The 30s tool body must have been DROPPED — it must not run to completion.
    // Give the Drop guard a brief grace window before checking both signals.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !harness.completed.load(Ordering::SeqCst),
        "spawn_only tool body ran to completion despite supervisor.cancel"
    );
    assert!(
        harness.dropped_while_running.load(Ordering::SeqCst),
        "supervisor.cancel flipped status without dropping the detached tool future"
    );
}

/// Codex #1429 P2: when a `SubAgentOutputRouter` is configured, the cancel arm
/// must run the SAME terminal teardown as the completion path
/// (`router.mark_terminal`). On the unfixed cancel arm the worker `return`ed
/// early, leaving the output handle stuck in the Running phase — so dashboards
/// show the task running forever and `AgentSummaryGenerator` (which does NOT
/// treat `Cancelled` as terminal) keeps polling an aborted task.
#[tokio::test]
async fn cancel_runs_terminal_teardown_for_routed_spawn_only_task() {
    let harness = SpawnOnlyHarness::new("interrupt-teardown", true).await;
    let router = harness.router.as_ref().unwrap();
    let task_id = harness.launch().await;
    assert!(
        !router.is_terminal(&task_id),
        "routed output handle should be Running before cancel"
    );
    harness.cancel_and_wait(&task_id).await;

    // The cancel arm must run the same router.mark_terminal teardown as the
    // completion path instead of leaving the output handle Running forever.
    let mut waited = Duration::ZERO;
    while !router.is_terminal(&task_id) && waited < Duration::from_secs(1) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert!(
        router.is_terminal(&task_id),
        "cancelled spawn_only task left its output handle Running"
    );
    assert!(
        !harness.completed.load(Ordering::SeqCst),
        "tool body ran to completion despite cancel"
    );
}
