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

#[tokio::test]
async fn supervisor_cancel_aborts_running_spawn_only_task() {
    let memory_dir = TempDir::new().unwrap();

    let entered = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicBool::new(false));
    let dropped_while_running = Arc::new(AtomicBool::new(false));
    let probe = HangingTool {
        name: "hang_bg",
        entered: entered.clone(),
        completed: completed.clone(),
        dropped_while_running: dropped_while_running.clone(),
        // Long enough that, absent cancellation, the body is still running
        // well past the bounded interrupt window we assert below.
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

    let agent =
        Agent::new(AgentId::new("interrupt-cancel"), llm, tools, memory).with_config(AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        });

    // Capture the supervisor-assigned task_id the moment the background task
    // is registered / starts running. spawn_only registers a `Spawned` row in
    // the foreground before `tokio::spawn`, so `on_change` fires with the id.
    let supervisor = agent.tool_registry().supervisor();
    let captured_id: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    {
        let captured_id = captured_id.clone();
        supervisor.set_on_change(move |task| {
            if task.tool_name == "hang_bg" {
                *captured_id.lock().unwrap() = Some(task.id.clone());
            }
        });
    }

    // The agent main loop returns immediately: spawn_only detaches and the
    // LLM's EndTurn finishes the foreground turn.
    let _ = agent
        .process_message("kick the hanging spawn_only pipeline", &[], vec![])
        .await
        .expect("agent loop should not error launching a spawn_only tool");

    // Wait for the background tool body to actually enter (bounded).
    let mut waited = Duration::ZERO;
    while !entered.load(Ordering::SeqCst) && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(10)).await;
        waited += Duration::from_millis(10);
    }
    assert!(
        entered.load(Ordering::SeqCst),
        "background spawn_only tool body never started"
    );

    let task_id = captured_id
        .lock()
        .unwrap()
        .clone()
        .expect("supervisor must have registered the spawn_only task id");

    // === THE INTERRUPT === cancel the running task, exactly as the serve
    // `turn/interrupt` path now does for a turn's spawn_only background work.
    supervisor
        .cancel(&task_id)
        .expect("cancel of a running task must succeed");

    // Within a BOUNDED window the detached worker must be aborted: its tool
    // future is dropped at the cancel safe-point, so `completed` never flips
    // and the supervisor record is terminal `Cancelled`.
    let mut waited = Duration::ZERO;
    let deadline = Duration::from_secs(3);
    loop {
        let task = supervisor.get_task(&task_id);
        let is_cancelled = task
            .as_ref()
            .map(|t| matches!(t.status, octos_agent::TaskStatus::Cancelled))
            .unwrap_or(false);
        if is_cancelled {
            break;
        }
        assert!(
            waited < deadline,
            "task did not reach terminal Cancelled within {deadline:?}; \
             status={:?}",
            task.map(|t| t.status)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }

    // The 30s tool body must have been DROPPED — it must not run to completion.
    // Give it a brief grace window so the dropped-future Drop guard can fire.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !completed.load(Ordering::SeqCst),
        "spawn_only tool body ran to completion despite supervisor.cancel — \
         the detached worker ignored the cancel token (turn interrupt cannot \
         break a running pipeline)"
    );

    // The load-bearing assertion: the tool future must actually have been
    // DROPPED mid-flight. On unfixed HEAD `supervisor.cancel` only flips the
    // dashboard status; the detached `tokio::spawn` body keeps sleeping its
    // full 30s and the sentinel is never dropped, so this fails.
    assert!(
        dropped_while_running.load(Ordering::SeqCst),
        "supervisor.cancel did NOT abort the detached spawn_only worker — its \
         tool future is still alive and running (a hung pipeline survives the \
         interrupt). Status flipped to Cancelled but the work never stopped."
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
    let memory_dir = TempDir::new().unwrap();
    let router_dir = TempDir::new().unwrap();

    let entered = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicBool::new(false));
    let dropped_while_running = Arc::new(AtomicBool::new(false));
    let probe = HangingTool {
        name: "hang_bg",
        entered: entered.clone(),
        completed: completed.clone(),
        dropped_while_running: dropped_while_running.clone(),
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

    // The load-bearing addition vs. the abort test: a real output router, so
    // the cancel arm actually has terminal teardown to run (or skip, pre-fix).
    // The worker seeds a startup line via `router.append` before the tool runs,
    // creating the handle in the Running phase.
    let router = Arc::new(octos_agent::SubAgentOutputRouter::new(router_dir.path()));

    let agent = Agent::new(AgentId::new("interrupt-teardown"), llm, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        })
        .with_subagent_output_router(router.clone());

    let supervisor = agent.tool_registry().supervisor();
    let captured_id: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    {
        let captured_id = captured_id.clone();
        supervisor.set_on_change(move |task| {
            if task.tool_name == "hang_bg" {
                *captured_id.lock().unwrap() = Some(task.id.clone());
            }
        });
    }

    let _ = agent
        .process_message("kick the hanging spawn_only pipeline", &[], vec![])
        .await
        .expect("agent loop should not error launching a spawn_only tool");

    let mut waited = Duration::ZERO;
    while !entered.load(Ordering::SeqCst) && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(10)).await;
        waited += Duration::from_millis(10);
    }
    assert!(
        entered.load(Ordering::SeqCst),
        "background spawn_only tool body never started"
    );

    let task_id = captured_id
        .lock()
        .unwrap()
        .clone()
        .expect("supervisor must have registered the spawn_only task id");

    // Precondition: the routed handle is Running (not terminal) before cancel.
    assert!(
        !router.is_terminal(&task_id),
        "routed output handle should be Running before cancel"
    );

    supervisor
        .cancel(&task_id)
        .expect("cancel of a running task must succeed");

    // Wait for terminal `Cancelled` status (bounded).
    let mut waited = Duration::ZERO;
    let deadline = Duration::from_secs(3);
    loop {
        let cancelled = supervisor
            .get_task(&task_id)
            .map(|t| matches!(t.status, octos_agent::TaskStatus::Cancelled))
            .unwrap_or(false);
        if cancelled {
            break;
        }
        assert!(
            waited < deadline,
            "task did not reach terminal Cancelled within {deadline:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }

    // THE P2 ASSERTION: the cancel arm must have run the terminal teardown, so
    // the routed output handle is flagged Terminal. Pre-fix the cancel arm
    // `return`ed before `router.mark_terminal`, leaving it stuck Running (the
    // worker runs the teardown just after the token fires, so poll briefly).
    let mut waited = Duration::ZERO;
    while !router.is_terminal(&task_id) && waited < Duration::from_secs(1) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert!(
        router.is_terminal(&task_id),
        "cancelled spawn_only task left its output handle in the Running phase — \
         the cancel arm skipped router.mark_terminal (codex #1429 P2)"
    );
    // The abort itself still holds (the tool body did not run to completion).
    assert!(
        !completed.load(Ordering::SeqCst),
        "tool body ran to completion despite cancel"
    );
}
