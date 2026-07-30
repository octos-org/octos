//! Shared test helpers: in-memory-ish store/fleet builders and scripted
//! mock LLM providers. Compiled only under `#[cfg(test)]`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_agent::sandbox::{NoSandbox, Sandbox};
use octos_core::{Message, SessionKey};
use octos_fleet::{
    AcceptanceCriterion, Fleet, FleetBudget, FleetKernelStore, LaunchOutcome, TaskSpec, Verifier,
    WorkerGrant,
};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;
use tokio::process::Command;

use crate::AgentFactory;

pub const EPOCH: u64 = 100;
pub const TTL: u64 = 10_000;
pub const PROJECTED: u64 = 100;
pub const NOW: u64 = 1_000;

pub fn controller() -> SessionKey {
    SessionKey::new("fleet", "keeper-1")
}

pub fn budget(cap: u64) -> FleetBudget {
    FleetBudget {
        token_budget: cap,
        tokens_reserved: 0,
        tokens_committed: 0,
        hard: false,
    }
}

/// A fresh kernel store in its own tempdir (kept alive by the returned guard).
pub async fn fresh_store() -> (TempDir, Arc<FleetKernelStore>) {
    let dir = TempDir::new().unwrap();
    let store = FleetKernelStore::open(dir.path()).await.unwrap();
    (dir, Arc::new(store))
}

/// A fresh episodic store in its own tempdir.
pub async fn fresh_memory() -> (TempDir, Arc<EpisodeStore>) {
    let dir = TempDir::new().unwrap();
    let memory = EpisodeStore::open(dir.path()).await.unwrap();
    (dir, Arc::new(memory))
}

/// A `TaskSpec` with the given deps + acceptance and the minimal (least-privilege)
/// worker grant — today's closed worker.
pub fn task_spec(id: &str, deps: &[&str], acceptance: Vec<AcceptanceCriterion>) -> TaskSpec {
    task_spec_granted(id, deps, acceptance, WorkerGrant::minimal())
}

/// A `TaskSpec` carrying an explicit [`WorkerGrant`] (for grant-driven tests).
pub fn task_spec_granted(
    id: &str,
    deps: &[&str],
    acceptance: Vec<AcceptanceCriterion>,
    grant: WorkerGrant,
) -> TaskSpec {
    TaskSpec {
        task_id: id.to_string(),
        title: format!("Task {id}"),
        detail: "do the thing".to_string(),
        deps: deps.iter().map(|s| s.to_string()).collect(),
        acceptance,
        grant,
    }
}

/// One hard `FileExists` acceptance criterion on `path`.
pub fn file_exists(path: &str) -> Vec<AcceptanceCriterion> {
    vec![AcceptanceCriterion {
        id: "file".to_string(),
        description: format!("{path} must be written"),
        verifier: Verifier::FileExists {
            path: path.to_string(),
        },
    }]
}

/// One `ValidatorRef` acceptance criterion — unsupported by the v1 executor,
/// so the gate must fail closed on it (never pass on the agent's self-report).
pub fn validator_ref(id: &str) -> Vec<AcceptanceCriterion> {
    vec![AcceptanceCriterion {
        id: "vref".to_string(),
        description: format!("validator {id} must pass"),
        verifier: Verifier::ValidatorRef { id: id.to_string() },
    }]
}

/// One `CommandExit(0)` acceptance criterion running `cmd` (whitespace argv,
/// NO shell — the validator runs the split program + args directly).
pub fn command_exit(cmd: &str) -> Vec<AcceptanceCriterion> {
    vec![AcceptanceCriterion {
        id: "cmd".to_string(),
        description: format!("`{cmd}` must exit 0"),
        verifier: Verifier::CommandExit {
            cmd: cmd.to_string(),
            code: 0,
        },
    }]
}

/// Create a fleet + plan from `tasks` (generous budget, `now = NOW`).
pub async fn create_fleet(
    store: Arc<FleetKernelStore>,
    fleet_id: &str,
    tasks: Vec<TaskSpec>,
) -> Fleet {
    Fleet::create(
        store,
        fleet_id,
        controller(),
        None,
        "default",
        budget(1_000_000),
        "objective",
        tasks,
        NOW,
    )
    .await
    .unwrap()
}

/// Launch a `Ready` child and return its attempt id (panics otherwise).
pub async fn launch(store: &FleetKernelStore, fleet_id: &str, task_id: &str) -> String {
    match store
        .launch_child(fleet_id, task_id, PROJECTED, NOW, EPOCH, TTL)
        .await
        .unwrap()
    {
        LaunchOutcome::Launched { attempt_id } => attempt_id,
        other => panic!("expected Launched for {task_id}, got {other:?}"),
    }
}

/// A test double for a REAL isolating sandbox backend: it runs commands
/// directly (like [`NoSandbox`]) but reports `is_noop() == false`. The
/// attempt-time fail-closed guard in `run_attempt` (H1) refuses to run the agent
/// under a no-op sandbox, so run_attempt tests thread THIS to exercise the agent
/// path; a test that wants the fail-closed path passes a genuine [`NoSandbox`].
pub struct MarkerSandbox;

impl Sandbox for MarkerSandbox {
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        NoSandbox.wrap_command(shell_command, cwd)
    }
}

/// Records how many times `chat` was invoked (shared with the caller). Used to
/// prove `run_attempt` did NOT build or run the agent — e.g. the H1 fail-closed
/// path terminates the attempt BEFORE any LLM call.
pub struct CountingProvider {
    pub calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for CountingProvider {
    async fn chat(&self, _m: &[Message], _t: &[ToolSpec], _c: &ChatConfig) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(end_turn("counted"))
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// An [`AgentFactory`] over `provider` and a throwaway episodic store. Threads a
/// [`MarkerSandbox`] (a real-isolating test double), NOT a no-op sandbox, so the
/// attempt-time fail-closed guard (H1) lets the agent run. The returned
/// [`TempDir`] must be held for the store's lifetime.
pub async fn factory_for(provider: Arc<dyn LlmProvider>) -> (TempDir, AgentFactory) {
    let (dir, memory) = fresh_memory().await;
    let factory = AgentFactory::new(
        provider,
        memory,
        Arc::new(|_, _| Arc::new(MarkerSandbox) as Arc<dyn Sandbox>),
    );
    (dir, factory)
}

fn usage() -> octos_llm::TokenUsage {
    octos_llm::TokenUsage {
        input_tokens: 7,
        output_tokens: 3,
        ..Default::default()
    }
}

fn end_turn(text: &str) -> ChatResponse {
    ChatResponse {
        content: Some(text.to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: usage(),
        provider_index: None,
    }
}

/// Always ends the turn with a successful text answer, calling NO tools.
/// Used to prove the acceptance gate rejects a run that produced no artifact.
pub struct SuccessProvider;

#[async_trait]
impl LlmProvider for SuccessProvider {
    async fn chat(&self, _m: &[Message], _t: &[ToolSpec], _c: &ChatConfig) -> Result<ChatResponse> {
        Ok(end_turn("done, nothing to write"))
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Writes `path` via the `write_file` tool on the first turn, then ends.
pub struct WriteFileProvider {
    pub path: String,
    calls: AtomicUsize,
}

impl WriteFileProvider {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for WriteFileProvider {
    async fn chat(&self, _m: &[Message], _t: &[ToolSpec], _c: &ChatConfig) -> Result<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_write".to_string(),
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({
                        "path": self.path,
                        "content": "fleet worker artifact\n",
                    }),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: usage(),
                provider_index: None,
            });
        }
        Ok(end_turn("wrote the artifact"))
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Sleeps past the attempt deadline on the first `chat`, so the `timeout`
/// wrapper in `run_attempt` fires.
pub struct SleepProvider {
    pub hold: Duration,
}

#[async_trait]
impl LlmProvider for SleepProvider {
    async fn chat(&self, _m: &[Message], _t: &[ToolSpec], _c: &ChatConfig) -> Result<ChatResponse> {
        tokio::time::sleep(self.hold).await;
        Ok(end_turn("finished after a long nap"))
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Records max observed concurrency: increments `active` on entry, folds it
/// into `max`, holds `hold`, then decrements and ends. Used to prove the
/// pool never runs more than `global_concurrency` attempts at once.
pub struct ConcurrencyProvider {
    pub active: Arc<AtomicUsize>,
    pub max: Arc<AtomicUsize>,
    pub hold: Duration,
}

#[async_trait]
impl LlmProvider for ConcurrencyProvider {
    async fn chat(&self, _m: &[Message], _t: &[ToolSpec], _c: &ChatConfig) -> Result<ChatResponse> {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.hold).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(end_turn("done"))
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}
