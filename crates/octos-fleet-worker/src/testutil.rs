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
    AcceptanceCriterion, Fleet, FleetBudget, FleetKernelStore, FsGrant, LaunchOutcome,
    NetworkGrant, TaskSpec, Verifier, WorkerGrant,
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

/// A COHERENT FULL-TRUST grant — `FsGrant::Host` + `NetworkGrant::Full` — the
/// operator decision that makes a task a WORKTREE worker on a git root (codex
/// fix #1: the worktree path requires BOTH lanes, so a `Host-FS + restricted
/// network` worker can't be a not-truly-isolated worktree worker).
pub fn host_grant() -> WorkerGrant {
    WorkerGrant {
        fs: FsGrant::Host,
        network: NetworkGrant::Full,
        ..WorkerGrant::minimal()
    }
}

/// A MIXED grant — `FsGrant::Host` but only `NetworkGrant::None` — which is NOT
/// full-trust, so the worktree path must REFUSE it (scratch fallback). Used to
/// prove the coherent-grant gate (codex fix #1).
pub fn host_fs_no_network_grant() -> WorkerGrant {
    WorkerGrant {
        fs: FsGrant::Host,
        network: NetworkGrant::None,
        ..WorkerGrant::minimal()
    }
}

/// Create a fleet whose controller workspace root is `root` (e.g. a real git
/// repo path), so the pool's worktree preflight engages. `None` reproduces the
/// scratch-workspace fallback.
pub async fn create_fleet_with_root(
    store: Arc<FleetKernelStore>,
    fleet_id: &str,
    root: Option<String>,
    tasks: Vec<TaskSpec>,
) -> Fleet {
    Fleet::create(
        store,
        fleet_id,
        controller(),
        root,
        "default",
        budget(1_000_000),
        "objective",
        tasks,
        NOW,
    )
    .await
    .unwrap()
}

/// Init a git repo at `dir` with one commit, so `HEAD` is valid for the
/// worktree `add -b … HEAD`. Returns `false` if git is unavailable (the caller
/// should skip the test). Mirrors the helper in `octos_core::git_worktree`.
pub fn git_init_repo(dir: &Path) -> bool {
    use std::process::Command as StdCommand;
    if StdCommand::new("git").arg("--version").output().is_err() {
        return false;
    }
    let ok = |args: &[&str]| {
        StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    ok(&["init", "-q"])
        && ok(&["config", "user.email", "fleet@test"])
        && ok(&["config", "user.name", "fleet-test"])
        && ok(&["config", "commit.gpgsign", "false"])
        && {
            std::fs::write(dir.join("seed.txt"), b"seed\n").unwrap();
            ok(&["add", "-A"]) && ok(&["commit", "-q", "-m", "seed"])
        }
}

/// Whether `refname` resolves in the git repo at `repo`.
pub fn git_ref_exists(repo: &Path, refname: &str) -> bool {
    use std::process::Command as StdCommand;
    StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show-ref", "--verify", "--quiet", refname])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether `file` exists in the tree of `branch` in the repo at `repo` (i.e. a
/// commit that added it landed on the branch). Survives checkout removal, so it
/// proves the worker's work is durable on the `fleet/*` branch, not just in the
/// (later-removed) checkout.
pub fn git_branch_contains_file(repo: &Path, branch: &str, file: &str) -> bool {
    use std::process::Command as StdCommand;
    StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-e", &format!("{branch}:{file}")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Writes `file` and `git commit`s it to the current branch via the closed
/// worker's `shell` tool on the first turn, then ends. Used to prove a fleet
/// worktree worker does REAL, committed repo work on its `fleet/*` branch.
pub struct GitCommitProvider {
    pub file: String,
    calls: AtomicUsize,
}

impl GitCommitProvider {
    pub fn new(file: &str) -> Self {
        Self {
            file: file.to_string(),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for GitCommitProvider {
    async fn chat(&self, _m: &[Message], _t: &[ToolSpec], _c: &ChatConfig) -> Result<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let command = format!(
                "printf 'fleet worker artifact\\n' > {file} && git add -A && \
                 git -c user.email=w@fleet -c user.name=w commit -q -m 'fleet task work'",
                file = self.file
            );
            return Ok(ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_commit".to_string(),
                    name: "shell".to_string(),
                    arguments: serde_json::json!({ "command": command }),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: usage(),
                provider_index: None,
            });
        }
        Ok(end_turn("committed the artifact"))
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
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
/// It also reports `supports_repo_git_write() == true` so it doubles a full-FS
/// backend (bwrap / full-read macOS) for the worktree-flow tests (codex fix #2b
/// terminates a worktree attempt whose resolved sandbox can't grant full-FS
/// write).
pub struct MarkerSandbox;

impl Sandbox for MarkerSandbox {
    fn supports_repo_git_write(&self) -> bool {
        true
    }

    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        NoSandbox.wrap_command(shell_command, cwd)
    }
}

/// A test double for a real isolating backend that does NOT support full-FS
/// write (docker / restricted-read macOS): non-noop but
/// `supports_repo_git_write() == false`. Used to prove codex fix #2b — a worktree
/// attempt whose RESOLVED sandbox degraded to non-full-FS-write is terminated.
pub struct RestrictedSandbox;

impl Sandbox for RestrictedSandbox {
    // Inherits the default `supports_repo_git_write() == false`.
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        NoSandbox.wrap_command(shell_command, cwd)
    }
}

/// A [`SandboxFactory`] whose backend reports it CANNOT grant full-FS write
/// (codex fix #2b). Threads a [`RestrictedSandbox`].
pub fn restricted_sandbox_factory() -> crate::SandboxFactory {
    Arc::new(|_, _| Arc::new(RestrictedSandbox) as Arc<dyn Sandbox>)
}

/// Plants a hanging `filter.*.clean` (a `sleep`) via `.gitattributes` +
/// `.git/config`, then ends — so the deliverable auto-commit's `git add -A`
/// triggers the clean filter and HANGS. Used to prove codex fix #4 (the bounded
/// sandboxed commit kills a hung filter at the deadline). Populate already ran
/// (clean tree) before this plants the filter, so only the later `git add`
/// hangs, not the populate.
pub struct HangCleanFilterProvider {
    calls: AtomicUsize,
}

impl Default for HangCleanFilterProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl HangCleanFilterProvider {
    pub fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for HangCleanFilterProvider {
    async fn chat(&self, _m: &[Message], _t: &[ToolSpec], _c: &ChatConfig) -> Result<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            // Define a clean filter that sleeps for a long time, map out.txt to
            // it, and create out.txt — so the settle `git add -A` hangs on the
            // clean filter until the bounded commit's deadline kills it.
            let command = "git config filter.hang.clean 'sleep 3000' && \
                 printf 'out.txt filter=hang\\n' > .gitattributes && \
                 printf 'data\\n' > out.txt"
                .to_string();
            return Ok(ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_plant".to_string(),
                    name: "shell".to_string(),
                    arguments: serde_json::json!({ "command": command }),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: usage(),
                provider_index: None,
            });
        }
        Ok(end_turn("planted the hanging filter"))
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
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
    factory_for_with(
        provider,
        Arc::new(|_, _| Arc::new(MarkerSandbox) as Arc<dyn Sandbox>),
    )
    .await
}

/// [`factory_for`] with an EXPLICIT sandbox factory (e.g. [`RestrictedSandbox`]
/// to exercise codex fix #2b's per-attempt full-FS-write verification).
pub async fn factory_for_with(
    provider: Arc<dyn LlmProvider>,
    sandbox_factory: crate::SandboxFactory,
) -> (TempDir, AgentFactory) {
    let (dir, memory) = fresh_memory().await;
    let factory = AgentFactory::new(provider, memory, sandbox_factory);
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

/// Calls the always-on `escalate` tool on the first turn (requesting a wider
/// grant), then ends. Proves an escalation mid-turn yields the attempt
/// NON-terminally. `then_sleep` optionally holds AFTER escalating so a test can
/// drive the turn to the DEADLINE and prove the escalation still wins
/// (determinism), rather than ending cleanly with EndTurn.
pub struct EscalateProvider {
    pub reason: String,
    pub then_sleep: Option<Duration>,
    calls: AtomicUsize,
}

impl EscalateProvider {
    pub fn new(reason: &str) -> Self {
        Self {
            reason: reason.to_string(),
            then_sleep: None,
            calls: AtomicUsize::new(0),
        }
    }

    /// After escalating, sleep `hold` so the attempt hits the deadline while the
    /// slot is already set — the escalation must still win.
    pub fn then_sleeping(reason: &str, hold: Duration) -> Self {
        Self {
            reason: reason.to_string(),
            then_sleep: Some(hold),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for EscalateProvider {
    async fn chat(&self, _m: &[Message], _t: &[ToolSpec], _c: &ChatConfig) -> Result<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_escalate".to_string(),
                    name: "escalate".to_string(),
                    arguments: serde_json::json!({
                        "reason": self.reason,
                        "requested_grant": {
                            "network": { "mode": "hosts", "hosts": ["example.com"] },
                            "tools": ["read_file", "write_file", "shell", "web_fetch"],
                        },
                    }),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: usage(),
                provider_index: None,
            });
        }
        if let Some(hold) = self.then_sleep {
            tokio::time::sleep(hold).await;
        }
        Ok(end_turn("escalated; awaiting the operator"))
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Writes `path` via `write_file`, then ends — but only when a specific grant
/// tool is available. Used to prove a fresh attempt after a grant widen rebuilds
/// with the NEW capabilities: it records which tool NAMES it was offered.
pub struct RecordingWriteProvider {
    pub path: String,
    pub seen_tools: Arc<std::sync::Mutex<Vec<String>>>,
    calls: AtomicUsize,
}

impl RecordingWriteProvider {
    pub fn new(path: &str, seen_tools: Arc<std::sync::Mutex<Vec<String>>>) -> Self {
        Self {
            path: path.to_string(),
            seen_tools,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for RecordingWriteProvider {
    async fn chat(&self, _m: &[Message], t: &[ToolSpec], _c: &ChatConfig) -> Result<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            // Record the tool names the fresh attempt was offered — the audit
            // point for "rebuilt from the widened grant".
            *self.seen_tools.lock().unwrap() = t.iter().map(|s| s.name.clone()).collect();
            return Ok(ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_write".to_string(),
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({
                        "path": self.path,
                        "content": "post-grant artifact\n",
                    }),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: usage(),
                provider_index: None,
            });
        }
        Ok(end_turn("wrote the artifact after the grant widen"))
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
