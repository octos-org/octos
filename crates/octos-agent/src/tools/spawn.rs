//! Spawn tool for background subagent execution.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use metrics::counter;
use octos_core::{AgentId, InboundMessage, SessionScope, Task, TaskContext, TaskKind, TaskResult};
use octos_llm::{ContextWindowOverride, LlmProvider, ProviderRouter};
use octos_memory::EpisodeStore;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::mcp_agent::{
    DispatchContextContract, DispatchOutcome, DispatchRequest, DispatchResponse,
    McpAgentBackendConfig, SharedBackend, build_backend_from_config, build_dispatch_event_payload,
    dispatch_with_metrics,
};
use super::{Tool, ToolPolicy, ToolRegistry, ToolResult};
use crate::file_state_cache::FileStateCache;
use crate::harness_events::{HarnessEvent, HarnessEventSink, write_event_to_sink};
use crate::prompt_context::PromptContextManager;
use crate::role_template::RoleTemplate;
use crate::sandbox::{SandboxConfig, create_sandbox};
use crate::subagent_output::SubAgentOutputRouter;
use crate::subagent_summary::AgentSummaryGenerator;
use crate::task_supervisor::{TaskSupervisor, TaskTerminalGuard};
use crate::workspace_git::{
    WorkspaceContractStatus, WorkspaceProjectKind,
    resolve_preferred_workspace_contract_artifact_path, resolve_workspace_contract_artifact_paths,
};
use crate::{Agent, AgentConfig, HookContext, HookExecutor, HookPayload, HookResult};

/// Default MCP tool name dispatched on the remote agent. Chosen to match
/// the `run_task` convention used by `claude mcp serve` and
/// `codex mcp serve` — configurable via
/// [`SpawnTool::with_mcp_agent_backend`] for runtimes that expose a
/// different entry point.
pub const DEFAULT_MCP_AGENT_TOOL_NAME: &str = "run_task";

/// Metadata passed to the parent runtime when a spawned child needs its own
/// caller-owned prompt context manager.
#[derive(Clone, Debug)]
pub struct ChildPromptContextRequest {
    pub parent_session_key: Option<String>,
    pub child_session_key: Option<String>,
    pub task_id: Option<String>,
    pub worker_id: String,
    pub task_label: String,
}

pub type ChildPromptContextManagerFactory =
    Arc<dyn Fn(ChildPromptContextRequest) -> Option<Arc<dyn PromptContextManager>> + Send + Sync>;

/// Guard C (issue #607): maximum nesting depth for `spawn`-within-`spawn`
/// invocations before [`SpawnTool::execute_with_context`] refuses further
/// dispatch. Measured against [`super::ToolContext::spawn_depth`], which
/// the spawn tool increments before forwarding into a child agent's
/// `TOOL_CTX`.
///
/// At depth 0 (top-level tool call) up through depth 3 (great-grandchild)
/// the spawn proceeds; an attempt at depth 4 surfaces the structured
/// `"spawn depth limit (4) exceeded; refusing further nesting"` error.
/// Bound chosen empirically: the longest legitimate workflow chain we
/// observed in production is parent → planner → coder → tts (depth 3).
pub const MAX_SPAWN_DEPTH: u8 = 4;

/// Callback for delivering background task results directly to the session actor.
/// Returns `true` if the result was delivered, `false` if the actor is dead
/// (caller should fall back to the InboundMessage relay path).
pub type BackgroundResultSender =
    Arc<dyn Fn(BackgroundResultPayload) -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

pub type ChildSessionLifecycleSender = Arc<
    dyn Fn(ChildSessionLifecyclePayload) -> futures::future::BoxFuture<'static, bool> + Send + Sync,
>;

pub type ChildToolFactory = Arc<dyn Fn() -> Arc<dyn Tool> + Send + Sync>;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WorkerIsolation {
    #[default]
    Shared,
    Worktree,
}

#[derive(Clone, Debug)]
struct WorkerWorktree {
    slug: String,
    branch: String,
    path: PathBuf,
}

impl WorkerWorktree {
    fn mark_status(&self, status: &str) {
        if let Err(error) = write_worker_worktree_status(self, status) {
            warn!(
                slug = %self.slug,
                branch = %self.branch,
                path = %self.path.display(),
                error = %error,
                "spawn: failed to update worker worktree status"
            );
        }
    }
}

/// RAII wrapper around a freshly-allocated [`WorkerWorktree`] (PR #1250
/// review, finding 1).
///
/// `allocate_worker_worktree` necessarily runs before the spawn tool's later
/// refusal points — provider resolution (`resolve_sub_provider?`), the sync
/// plugin/tool availability `?` returns, the background fanout-cap refusal —
/// because both spawn branches need the child working dir up front. Each of
/// those refusals returns without ever starting a worker; dropping an armed
/// guard prunes the just-created worktree + branch so a refused spawn leaves
/// `git worktree list` (and `.octos/work`) unchanged. Leaking instead would
/// be permanent: the `octos clean` sweep deliberately skips directories that
/// are still registered as live worktrees. Call [`Self::disarm`] at the real
/// handoff — the point where the worker actually starts.
struct WorkerWorktreeGuard {
    repo_root: PathBuf,
    worktree: Option<WorkerWorktree>,
}

impl WorkerWorktreeGuard {
    fn new(repo_root: PathBuf, worktree: WorkerWorktree) -> Self {
        Self {
            repo_root,
            worktree: Some(worktree),
        }
    }

    fn worktree(&self) -> &WorkerWorktree {
        self.worktree
            .as_ref()
            .expect("worker worktree guard is armed until disarmed")
    }

    /// Hand ownership of the worktree to the worker: after this the guard
    /// no longer prunes on drop.
    fn disarm(mut self) -> WorkerWorktree {
        self.worktree
            .take()
            .expect("worker worktree guard disarmed twice")
    }
}

impl Drop for WorkerWorktreeGuard {
    fn drop(&mut self) {
        if let Some(worktree) = self.worktree.take() {
            prune_worker_worktree(&self.repo_root, &worktree);
        }
    }
}

/// Best-effort removal of a refused worker's worktree and branch. Failures
/// are logged, not propagated — this runs on refusal paths (and in `Drop`)
/// where the spawn refusal itself must surface to the caller.
fn prune_worker_worktree(repo_root: &Path, worktree: &WorkerWorktree) {
    // `--force` clears the untracked `.octos/worker-worktree.json` status
    // marker; the checkout is otherwise fresh.
    match Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "remove", "--force"])
        .arg(&worktree.path)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            warn!(
                path = %worktree.path.display(),
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "spawn: git worktree remove failed for refused spawn; \
                 falling back to manual prune"
            );
            if worktree.path.exists() {
                if let Err(error) = std::fs::remove_dir_all(&worktree.path) {
                    warn!(
                        path = %worktree.path.display(),
                        error = %error,
                        "spawn: failed to remove refused worker worktree directory"
                    );
                }
            }
            let _ = Command::new("git")
                .arg("-C")
                .arg(repo_root)
                .args(["worktree", "prune"])
                .output();
        }
        Err(error) => {
            warn!(
                path = %worktree.path.display(),
                error = %error,
                "spawn: failed to run git worktree remove for refused spawn"
            );
        }
    }
    match Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["branch", "-D"])
        .arg(&worktree.branch)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            branch = %worktree.branch,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "spawn: failed to delete refused worker branch"
        ),
        Err(error) => warn!(
            branch = %worktree.branch,
            error = %error,
            "spawn: failed to run git branch -D for refused spawn"
        ),
    }
}

fn validate_worker_worktree_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() {
        return Err("worker worktree slug must not be empty".to_string());
    }
    if slug.len() > 64 {
        return Err("worker worktree slug must be at most 64 characters".to_string());
    }
    if slug.starts_with('/') || slug.starts_with('\\') || slug.contains('\\') {
        return Err("worker worktree slug must be relative and use '/' separators".to_string());
    }
    for segment in slug.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err("worker worktree slug contains an unsafe path segment".to_string());
        }
        if !segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        {
            return Err(format!(
                "worker worktree slug segment {segment:?} contains unsupported characters"
            ));
        }
    }
    Ok(())
}

fn git_stdout(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .wrap_err_with(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        return Err(eyre::eyre!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_ref_exists(repo: &Path, refname: &str) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show-ref", "--verify", "--quiet", refname])
        .status()
        .wrap_err("failed to run git show-ref")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(eyre::eyre!("git show-ref exited with {status}")),
    }
}

/// Allocate a dedicated git worktree for a spawned worker.
///
/// Validation runs BEFORE `git worktree add` (PR #1250 review, finding 2):
/// the `.octos` / `.octos/work` components must not be symlinks and the
/// created work root must canonically resolve under the canonical repository
/// root — this holds even when the caller carries no [`SessionScope`] at
/// all. When a parent scope IS present, the planned worktree path must
/// additionally pass [`SessionScope::with_workspace`] before creation, so a
/// scope refusal never leaves a checkout behind (let alone one outside the
/// session root).
///
/// Returns the allocation armed inside a [`WorkerWorktreeGuard`] (finding 1)
/// plus the child scope precomputed from the validated path. The caller must
/// [`WorkerWorktreeGuard::disarm`] at the real worker handoff; any earlier
/// refusal drops the guard and prunes the worktree + branch.
/// Whether `dir` is inside a git work tree. Used to preflight worktree
/// isolation so a non-git workspace produces a clear, actionable error instead
/// of leaking git's raw `fatal: not a git repository`. Any failure to run git
/// (missing binary, permissions) resolves to `false` here — the caller's error
/// message covers "not a git repository" as the actionable remedy, and a truly
/// missing git surfaces its own error on the subsequent `git worktree add`.
fn is_inside_git_work_tree(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true")
        .unwrap_or(false)
}

fn allocate_worker_worktree(
    parent_working_dir: &Path,
    worker_id: &AgentId,
    parent_scope: Option<&Arc<SessionScope>>,
) -> Result<(WorkerWorktreeGuard, Option<Arc<SessionScope>>)> {
    // Preflight: worktree isolation is only possible inside a git repository.
    // Without this, a non-git workspace fell straight into
    // `git rev-parse --show-toplevel` and surfaced the raw
    // `fatal: not a git repository` — cryptic, and the model wasted a spawn
    // round reverse-engineering the remedy. Give it an actionable message so it
    // re-dispatches with `isolation: shared`/`none` on the first try.
    if !is_inside_git_work_tree(parent_working_dir) {
        return Err(eyre::eyre!(
            "worktree isolation requires the workspace ({}) to be a git repository, but it is not. \
             Re-dispatch the spawn with isolation: shared (or none), or start the session inside a git repo.",
            parent_working_dir.display()
        ));
    }
    let repo_root = PathBuf::from(git_stdout(
        parent_working_dir,
        &["rev-parse", "--show-toplevel"],
    )?);
    let canonical_repo_root = std::fs::canonicalize(&repo_root).wrap_err_with(|| {
        format!(
            "failed to canonicalize repository root {}",
            repo_root.display()
        )
    })?;
    let base_slug = worker_id.to_string();
    let work_root = repo_root.join(".octos").join("work");
    // Finding 2: refuse symlinked components BEFORE creating anything. A
    // symlinked `.octos` or `.octos/work` would make `create_dir_all` (for a
    // dangling link) and `git worktree add` write outside the repository —
    // and thus outside the session root — before any validation ran.
    for component in [repo_root.join(".octos"), work_root.clone()] {
        let is_symlink = component
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink());
        if is_symlink {
            return Err(eyre::eyre!(
                "worker worktree root component {} is a symlink; refusing to \
                 create worker worktrees through it",
                component.display()
            ));
        }
    }
    // Finding 2: session-scope validation BEFORE any write — even the
    // `.octos/work` root itself must sit inside the session root, or the
    // spawn is refused with nothing created at all (not even the empty
    // directory chain `create_dir_all` would otherwise leave behind). The
    // per-attempt validation inside the loop below then binds the actual
    // worker path.
    scoped_child_session_scope(parent_scope, &work_root).map_err(|error| {
        eyre::eyre!(
            "worker worktree root {} rejected by session scope: {error}",
            work_root.display()
        )
    })?;
    std::fs::create_dir_all(&work_root).wrap_err_with(|| {
        format!(
            "failed to create worker worktree root {}",
            work_root.display()
        )
    })?;
    // Post-create re-verification: the canonical work root must stay under
    // the canonical repository root. This closes the check/create race (a
    // symlink swapped in between the check above and `create_dir_all`) and
    // catches any resolution surprise a lexical join would hide.
    let canonical_work_root = std::fs::canonicalize(&work_root).wrap_err_with(|| {
        format!(
            "failed to canonicalize worker worktree root {}",
            work_root.display()
        )
    })?;
    if !canonical_work_root.starts_with(&canonical_repo_root) {
        return Err(eyre::eyre!(
            "worker worktree root {} resolves to {}, outside the repository root {}; \
             refusing symlink escape",
            work_root.display(),
            canonical_work_root.display(),
            canonical_repo_root.display()
        ));
    }

    for attempt in 0..=32 {
        let slug = if attempt == 0 {
            base_slug.clone()
        } else {
            format!("{base_slug}-{attempt}")
        };
        validate_worker_worktree_slug(&slug).map_err(eyre::Report::msg)?;
        let branch = format!("octos/worker/{slug}");
        let refname = format!("refs/heads/{branch}");
        // Plan against the canonical work root so the scope check below,
        // `git worktree add`, and every later status write all target the
        // same verified physical location.
        let path = canonical_work_root.join(&slug);
        if path.exists() || git_ref_exists(&repo_root, &refname)? {
            continue;
        }
        // Finding 2: session-scope validation BEFORE creation.
        // `SessionScope::with_workspace` requires the planned path under the
        // scope root by canonical form; a worktree that would land outside
        // the session root is refused here, with nothing created. (The path
        // only varies by slug suffix across attempts, so the first rejection
        // is authoritative — no point retrying other slugs.)
        let child_scope = scoped_child_session_scope(parent_scope, &path).map_err(|error| {
            eyre::eyre!(
                "worker worktree path {} rejected by session scope: {error}",
                path.display()
            )
        })?;
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .args(["worktree", "add", "-b"])
            .arg(&branch)
            .arg(&path)
            .arg("HEAD")
            .output()
            .wrap_err("failed to run git worktree add")?;
        if !output.status.success() {
            return Err(eyre::eyre!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let allocation = WorkerWorktree { slug, branch, path };
        allocation.mark_status("spawned");
        return Ok((WorkerWorktreeGuard::new(repo_root, allocation), child_scope));
    }

    Err(eyre::eyre!(
        "could not allocate a unique worker worktree slug for {base_slug:?}"
    ))
}

fn write_worker_worktree_status(worktree: &WorkerWorktree, status: &str) -> Result<()> {
    let state_dir = worktree.path.join(".octos");
    std::fs::create_dir_all(&state_dir)?;
    let payload = serde_json::json!({
        "schema_version": 1,
        "agent_id": worktree.slug,
        "branch": worktree.branch,
        "path": worktree.path,
        "status": status,
    });
    std::fs::write(
        state_dir.join("worker-worktree.json"),
        serde_json::to_vec_pretty(&payload)?,
    )?;
    Ok(())
}

fn scoped_child_session_scope(
    parent_scope: Option<&Arc<SessionScope>>,
    working_dir: &Path,
) -> Result<Option<Arc<SessionScope>>> {
    parent_scope
        .map(|scope| {
            scope
                .as_ref()
                .clone()
                .with_workspace(working_dir.to_path_buf())
                .map(Arc::new)
                .map_err(|error| eyre::eyre!(error))
        })
        .transpose()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundResultKind {
    Notification,
    Report,
}

#[derive(Debug, Clone)]
pub struct BackgroundResultPayload {
    pub task_label: String,
    pub content: String,
    pub kind: BackgroundResultKind,
    /// Media to retain on the durable transcript row for this completion.
    /// For the `NotConfigured` `send_file` fallback this stays `vec![]`
    /// because each file already has its own per-file transcript companion.
    /// For the `Satisfied` workspace-contract path it carries `output_files`
    /// directly (no separate per-file rows on that path).
    pub media: Vec<String>,
    /// M10 Phase 5a: media to surface on the canonical v2
    /// background-child envelope, independently of the transcript row.
    ///
    /// Why two fields: the per-file companions remain durable transcript
    /// data but are suppressed from UI projection; the child envelope MUST
    /// carry the file URLs or the completion renders as text-only. Splitting
    /// transcript media from projection media keeps the durable record and
    /// canonical presentation independent.
    ///
    /// Empty `Vec` (default) means "no projection-only attachments";
    /// the canonical child envelope falls back to `media` for the
    /// `Satisfied` path. Populated explicitly only by the
    /// `NotConfigured` success branch in `execution.rs`.
    pub envelope_media: Vec<String>,
    /// M8.10 follow-up (#649): the user message's `client_message_id` that
    /// originated this background task. Carries through to the late-arriving
    /// outbound's `metadata.thread_id` so the API channel can stamp SSE
    /// events with the originating turn — NOT whatever the per-chat sticky
    /// map happens to hold when the background task finally finalises.
    /// `None` for legacy callers and tests that don't track origination.
    pub originating_thread_id: Option<String>,
    /// M10 Phase 1: the task supervisor `TaskId` for the spawn_only task
    /// that produced this completion. Surfaced on the wire as
    /// `TurnSpawnCompleteEvent.task_id` so the client can attribute the
    /// new bubble to a specific background task (and, in Phase 4, drive
    /// `read_task_output` against it). `None` for legacy callers and
    /// tests that do not register tasks with the supervisor.
    pub task_id: Option<String>,
    /// Originating `tool_call_id` (the spawn_only tool invocation that
    /// produced this background task). Surfaced on the wire as
    /// [`octos_core::ui_protocol::TurnSpawnCompleteEvent::tool_call_id`]
    /// so the client can flip the in-flight chip from spinner to
    /// checkmark directly off the envelope, without a race against a
    /// `task/updated` watcher that builds `task_id → tool_call_id`
    /// post-hoc. `None` for legacy callers and tests that do not track
    /// the originating call.
    pub tool_call_id: Option<String>,
    /// Issue #960 fix (M10 Phase 4 plumbing): the originating user
    /// message's `client_message_id` (cmid) — the same value the
    /// supervisor records as
    /// [`crate::task_supervisor::BackgroundTask::originating_client_message_id`]
    /// and that the M8.9 recovery path threads onto its synthetic turn.
    /// Surfaces on the wire as
    /// [`octos_core::ui_protocol::TurnSpawnCompleteEvent::response_to_client_message_id`]
    /// so the SPA reducer can anchor the new assistant bubble to the
    /// parent user prompt instead of falling back to thread-map heuristics
    /// (the bundle's `subSpawnComplete` handler bails when that lookup
    /// misses — issue #960 root cause). For gateway-style channels the
    /// reporter binds the real per-user `cmid`; for the WS standalone-turn
    /// path the reporter binds the originating `TurnId` (a UUID) and the
    /// SPA already keys its thread-map on that same value, so the wire
    /// identity round-trips correctly in both shapes. `None` for legacy
    /// callers and tests that do not track origination.
    pub originating_client_message_id: Option<String>,
    /// C1 step 3: the terminal supervisor status (`Completed` / `Failed` /
    /// `Cancelled`) for the spawn_only task that produced this completion.
    /// Set at the same call sites that invoke `mark_completed` /
    /// `mark_failed`, so the session actor can read an explicit status
    /// instead of inferring success from the rendered `"✗"` / `"✅"` content
    /// heuristic. Carried alongside `task_id` so the actor can attribute the
    /// terminal state to a specific background task. `None` for legacy
    /// callers and tests that do not track the terminal status.
    pub terminal_status: Option<crate::task_supervisor::TaskStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildSessionLifecycleKind {
    Spawned,
    Completed,
    RetryableFailed,
    TerminalFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildSessionFailureAction {
    Retry,
    Escalate,
}

#[derive(Debug, Clone)]
pub struct ChildSessionLifecyclePayload {
    pub kind: ChildSessionLifecycleKind,
    pub task_id: String,
    pub task_label: String,
    pub instruction: String,
    pub parent_session_key: String,
    pub child_session_key: String,
    pub workflow_kind: Option<String>,
    pub current_phase: Option<String>,
    pub output_files: Vec<String>,
    pub failure_action: Option<ChildSessionFailureAction>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowTerminalOutputPolicy {
    deliver_final_artifact_only: bool,
    forbid_intermediate_files: bool,
    required_artifact_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowMetadata {
    workflow_kind: String,
    current_phase: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_output: Option<WorkflowTerminalOutputPolicy>,
    /// Coarse progress fraction in [0.0, 1.0] for this phase. Populated
    /// on every workflow_runtime-driven `mark_runtime_state` so the
    /// dashboard `runtime_detail.progress` field is non-null even for
    /// workflows whose internal tools (e.g. `run_pipeline`) do not emit
    /// per-event `HarnessEvent::progress`. Increments roughly with phase
    /// transitions: 0.05 at workflow start (`research`/initial phase),
    /// 0.95 when the runtime advances to `deliver_result`. The
    /// task_supervisor's [`mark_completed`] path lets the lifecycle
    /// state speak for terminal completion; we do not synthesize a
    /// 1.0 sentinel here to avoid stepping on real progress events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress: Option<f64>,
}

fn is_retryable_child_failure(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "token budget exceeded",
        "timed out",
        "timeout",
        "temporarily",
        "retry",
        "rate limit",
        "connection reset",
        "overloaded",
        "unavailable",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn classify_child_session_lifecycle_kind(
    result: &Result<octos_core::TaskResult>,
) -> ChildSessionLifecycleKind {
    match result {
        Ok(task_result) if task_result.success => ChildSessionLifecycleKind::Completed,
        Ok(task_result) if is_retryable_child_failure(&task_result.output) => {
            ChildSessionLifecycleKind::RetryableFailed
        }
        Ok(_) => ChildSessionLifecycleKind::TerminalFailed,
        Err(error) if is_retryable_child_failure(&error.to_string()) => {
            ChildSessionLifecycleKind::RetryableFailed
        }
        Err(_) => ChildSessionLifecycleKind::TerminalFailed,
    }
}

fn child_session_lifecycle_kind_label(kind: ChildSessionLifecycleKind) -> &'static str {
    match kind {
        ChildSessionLifecycleKind::Spawned => "spawned",
        ChildSessionLifecycleKind::Completed => "completed",
        ChildSessionLifecycleKind::RetryableFailed => "retryable_failed",
        ChildSessionLifecycleKind::TerminalFailed => "terminal_failed",
    }
}

fn child_session_failure_action(
    kind: ChildSessionLifecycleKind,
) -> Option<ChildSessionFailureAction> {
    match kind {
        ChildSessionLifecycleKind::Spawned | ChildSessionLifecycleKind::Completed => None,
        ChildSessionLifecycleKind::RetryableFailed => Some(ChildSessionFailureAction::Retry),
        ChildSessionLifecycleKind::TerminalFailed => Some(ChildSessionFailureAction::Escalate),
    }
}

fn child_session_failure_action_label(action: ChildSessionFailureAction) -> &'static str {
    match action {
        ChildSessionFailureAction::Retry => "retry",
        ChildSessionFailureAction::Escalate => "escalate",
    }
}

fn record_child_session_lifecycle(kind: ChildSessionLifecycleKind, outcome: &'static str) {
    counter!(
        "octos_child_session_lifecycle_total",
        "kind" => child_session_lifecycle_kind_label(kind).to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

async fn dispatch_child_session_lifecycle(
    sender: Option<&ChildSessionLifecycleSender>,
    payload: ChildSessionLifecyclePayload,
) -> bool {
    match sender {
        Some(sender) => sender(payload).await,
        None => false,
    }
}

fn background_result_kind_label(kind: BackgroundResultKind) -> &'static str {
    match kind {
        BackgroundResultKind::Notification => "notification",
        BackgroundResultKind::Report => "report",
    }
}

/// Frame a child's task with an unambiguous sub-agent identity so a weaker
/// model does not adopt the parent's perspective from the forked
/// conversation context (#1704). Leads with WHO the child is and that the
/// surrounding history is background, then the task itself.
fn frame_subagent_task(label: &str, task: &str) -> String {
    format!(
        "You are a delegated SUB-AGENT named \"{label}\". Any conversation \
         history in your context is BACKGROUND from the parent that spawned \
         you — do NOT respond to it, summarize it, or adopt its point of \
         view. Your ONE job is the task below; execute it and produce its \
         deliverable.\n\n=== YOUR TASK ===\n{task}"
    )
}

/// Detect a role/task capability mismatch (#1704 item 3, #1707 follow-up):
/// a read-only role (its allow-list has no write/exec surface) paired with a
/// task that demands writing a file or running a command. Returns a note to
/// append to the child's instructions so it delivers findings as its final
/// TEXT instead of silently "completing" without the impossible artifact
/// (mini4: `role:"reviewer"` children asked to "clone and write a review"
/// burned whole runs, then the parent hallucinated a fix).
///
/// `effective_tools` is the post-role, post-manifest allow-list. An empty
/// list means "all builtins" — no restriction — so no mismatch.
fn role_task_capability_warning(effective_tools: &[String], task: &str) -> Option<String> {
    if effective_tools.is_empty() {
        return None; // unconstrained: has everything
    }
    let has = |names: &[&str]| {
        effective_tools.iter().any(|t| {
            names.contains(&t.as_str())
                || t == "group:fs"
                || t == "group:runtime"
                || t.contains('*')
        })
    };
    let can_write = has(&["write_file", "edit_file", "apply_patch", "diff_edit"]);
    let can_exec = has(&["shell", "bash", "exec_command"]);
    let lower = task.to_lowercase();
    let wants_write = [
        "write ",
        "save ",
        "create ",
        "emit ",
        "produce ",
        ".md",
        "report to",
    ]
    .iter()
    .any(|kw| lower.contains(kw));
    let wants_exec = ["clone", "git ", "run ", "install", "build", "compile"]
        .iter()
        .any(|kw| lower.contains(kw));
    let mut gaps = Vec::new();
    if wants_write && !can_write {
        gaps.push("write files (no write_file/edit_file)");
    }
    if wants_exec && !can_exec {
        gaps.push("run shell commands such as `git clone` (no shell/bash)");
    }
    if gaps.is_empty() {
        return None;
    }
    Some(format!(
        "CAPABILITY NOTE: your task implies actions you cannot perform — you cannot {}. \
         Do NOT pretend to; instead do everything your tools DO allow (read, search, \
         fetch) and deliver your full result as your FINAL TEXT ANSWER so the parent \
         can capture it. State plainly which steps you could not perform.",
        gaps.join(" and ")
    ))
}

/// Streams a background spawn child's transcript — assistant text, tool
/// starts/completions, file modifications — into the parent's
/// [`SubAgentOutputRouter`] file for its task.
///
/// Without this the detached child runs with the default `SilentReporter`
/// and its entire transcript is dropped: the router file only ever received
/// spawn_only TOOL output (`execution.rs`), never a spawn CHILD's loop
/// events, so `read_task_output` on a running child read an absent file and
/// the agent view could show status but no work (mini4 re-review forensic,
/// 2026-07-17). The router enforces per-task and total byte caps, so a
/// chatty child is bounded.
///
/// The `router_session_id` must mirror `read_task_output`'s lookup:
/// `agent:{task.tool_call_id}` where the spawn registers with
/// `tool_call_id = "spawn-{worker_id}"`.
///
/// When an `on_stream_chunk` callback is set, live `StreamChunk` events are
/// forwarded there so the caller (the WS/serve layer) can emit
/// `agent/output/delta` directly — bypassing the heavy `on_change` path
/// (which persists a snapshot on every fire). This gives the TUI agent dock
/// live streaming output for running background children without per-token
/// persistence overhead (codex plan review: "the proposed per-token
/// persistence/on_change fan-out is too heavy"). The WS layer emits via
/// `send_notification_ephemeral(UiNotification::AgentOutputDelta(...))`.
/// Live child-stream callback: `(agent_id, start_offset, text_delta)` —
/// `start_offset` is the cumulative streamed-bytes offset BEFORE the delta
/// (the window's first byte), `u64` end to end to match
/// `OutputCursor::offset` on the wire.
type ChildStreamCallback = Arc<dyn Fn(&str, u64, &str) + Send + Sync>;

struct SpawnChildTranscriptReporter {
    router: Arc<SubAgentOutputRouter>,
    router_session_id: String,
    task_id: String,
    /// Cumulative byte count of streamed text so far. Incremented per
    /// `StreamChunk`; the callback receives the value from BEFORE the
    /// increment, i.e. the START offset of the delta's window — the same
    /// convention every other `cursor` producer uses
    /// (`TaskOutputDeltaTracker`, the agent_orchestrator read RPCs all
    /// report `cursor` = window start). Monotonic, so clients can detect
    /// gaps / reorder if chunks ever arrive out of order (e.g. across
    /// reconnects). Atomic because `report(&self)` is `&self` and the
    /// reporter is shared across the child's tokio task(s). `u64` end to
    /// end so no lossy narrowing hides between here and
    /// `OutputCursor::offset` (`u64`) on the wire.
    stream_offset: AtomicU64,
    /// Optional callback for live `StreamChunk` forwarding. Called directly
    /// from the reporter thread with `(agent_id, cursor_offset, text_delta)`
    /// — the caller owns the emit path (e.g. emitting
    /// `agent/output/delta` via `send_notification_ephemeral` with a
    /// properly-formed `AgentOutputDeltaEvent` keyed on the child's
    /// `agent_id`). The `agent_id` is the spawn's `task_id` (the same id
    /// surfaced via `TurnSpawnCompleteEvent` / the agent dock).
    /// `cursor_offset` is the cumulative byte offset BEFORE this chunk
    /// (i.e. the start-offset of this delta's window) — matches the
    /// start-offset convention of every sibling `OutputCursor` producer.
    on_stream_chunk: Option<ChildStreamCallback>,
}

impl crate::progress::ProgressReporter for SpawnChildTranscriptReporter {
    fn report(&self, event: crate::progress::ProgressEvent) {
        use crate::progress::ProgressEvent;
        let line = match event {
            ProgressEvent::Response { content, .. } => {
                if content.trim().is_empty() {
                    return;
                }
                format!("{content}\n")
            }
            ProgressEvent::ToolStarted { name, .. } => format!("[tool] {name}\n"),
            ProgressEvent::ToolCompleted {
                name,
                success,
                output_preview,
                ..
            } => {
                let status = if success { "ok" } else { "FAILED" };
                let first = output_preview.lines().next().unwrap_or_default();
                let first = octos_core::truncated_utf8(first, 200, "…");
                format!("[tool {status}] {name} — {first}\n")
            }
            ProgressEvent::FileModified { path } => format!("[file modified] {path}\n"),
            // StreamChunk: live text goes ONLY to the direct
            // on_stream_chunk callback (if set) so the WS layer can emit
            // agent/output/delta WITHOUT going through on_change (which
            // persists a snapshot on every fire — too heavy per-token per
            // codex review). Deliberately NO router append here: the
            // Response arm below already appends each iteration's full
            // text, so writing the same bytes per-chunk would land every
            // child message TWICE in `<task_id>.out` (this exact
            // duplication is what the original drop-comment on this arm
            // warned about). The router file stays the Response-based
            // durable record; `task/output/delta` is produced from
            // supervisor progress events, not router appends, so no live
            // consumer loses anything.
            //
            // Delivery note (codex review): a fast LLM child can fire
            // this 100+ times/sec. There is no per-token coalescing here,
            // and the ephemeral WS send is LOSSY: `try_enqueue` onto the
            // bounded channel drops the frame on full (BackpressureDrop)
            // rather than blocking the producer. Dropped deltas are
            // acceptable for a live tail (the durable record is the
            // Response append); if loss under N parallel children proves
            // user-visible, add a ~16ms coalescing debounce at the
            // callback site.
            ProgressEvent::StreamChunk { text, .. } => {
                if !text.is_empty() {
                    // `fetch_add` returns the PREVIOUS value: the
                    // cumulative streamed-bytes offset BEFORE this chunk,
                    // i.e. the START offset of the delta's window —
                    // matching every sibling `cursor` producer
                    // (TaskOutputDeltaTracker, the agent_orchestrator
                    // read RPCs). Monotonic, so clients can detect gaps /
                    // reorder on reconnect.
                    let start_offset = self
                        .stream_offset
                        .fetch_add(text.len() as u64, Ordering::Relaxed);
                    if let Some(ref cb) = self.on_stream_chunk {
                        // Pass `(task_id, cursor_offset, text)` so the WS
                        // layer can construct a properly-keyed
                        // `AgentOutputDeltaEvent` (the dock correlates live
                        // output with the spawned agent by `task_id`).
                        cb(self.task_id.as_str(), start_offset, text.as_str());
                    }
                }
                return;
            }
            // Thinking/LlmStatus/etc. are cadence noise for a transcript.
            _ => return,
        };
        // Best-effort: a full router (byte caps) or IO error must never
        // disturb the child's run.
        let _ = self
            .router
            .append(&self.router_session_id, &self.task_id, line.as_bytes());
    }
}

fn record_result_delivery(path: &'static str, outcome: &'static str, kind: BackgroundResultKind) {
    counter!(
        "octos_result_delivery_total",
        "path" => path.to_string(),
        "outcome" => outcome.to_string(),
        "kind" => background_result_kind_label(kind).to_string()
    )
    .increment(1);
}

fn record_terminal_result_reason(kind: BackgroundResultKind, reason: &'static str) {
    counter!(
        "octos_terminal_result_reason_total",
        "kind" => background_result_kind_label(kind).to_string(),
        "reason" => reason.to_string()
    )
    .increment(1);
}

fn record_retry(reason: &'static str) {
    counter!("octos_retry_total", "reason" => reason.to_string()).increment(1);
}

async fn emit_lifecycle_hook(hooks: Option<&Arc<HookExecutor>>, payload: HookPayload) {
    let Some(hooks) = hooks else {
        return;
    };
    let event = payload.event;
    match hooks.run(event, &payload).await {
        HookResult::Allow => {}
        HookResult::Modified(_) => {
            warn!(event = ?event, "lifecycle hook attempted to modify payload; ignoring");
        }
        // Context injection is a `user_prompt_submit`-only outcome; a spawn
        // lifecycle event never produces it, but the match must be exhaustive.
        HookResult::Context(_) => {}
        HookResult::Deny(reason) => {
            warn!(
                event = ?event,
                reason,
                "lifecycle hook attempted to deny a non-blocking event"
            );
        }
        HookResult::Error(error) => {
            warn!(event = ?event, error, "lifecycle hook failed");
        }
    }
}

fn parse_modified_spawn_verify_output_files(
    modified: serde_json::Value,
) -> std::result::Result<Vec<PathBuf>, String> {
    let files = match modified {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(mut object) => object
            .remove("output_files")
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| {
                "before_spawn_verify hook must return {\"output_files\": [...]} or a JSON string array"
                    .to_string()
            })?,
        _ => {
            return Err(
                "before_spawn_verify hook must return {\"output_files\": [...]} or a JSON string array"
                    .to_string(),
            )
        }
    };

    files
        .into_iter()
        .map(|value| match value {
            serde_json::Value::String(path) => Ok(PathBuf::from(path)),
            _ => Err("before_spawn_verify output_files entries must be strings".to_string()),
        })
        .collect()
}

async fn run_before_spawn_verify_hook(
    hooks: Option<&Arc<HookExecutor>>,
    payload: HookPayload,
) -> std::result::Result<Vec<PathBuf>, String> {
    let default_files = payload
        .output_files
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let Some(hooks) = hooks else {
        return Ok(default_files);
    };
    let event = payload.event;

    match hooks.run(event, &payload).await {
        HookResult::Allow => Ok(default_files),
        HookResult::Modified(modified) => parse_modified_spawn_verify_output_files(modified),
        // `before_spawn_verify` never yields context injection (that is a
        // `user_prompt_submit`-only outcome); treat it like a plain allow.
        HookResult::Context(_) => Ok(default_files),
        HookResult::Deny(reason) => Err(reason),
        HookResult::Error(error) => {
            warn!(
                event = ?event,
                error,
                "pre-verify lifecycle hook failed; continuing with runtime output files"
            );
            Ok(default_files)
        }
    }
}

/// Tool that spawns background worker agents for long-running tasks.
pub struct SpawnTool {
    llm: Arc<dyn LlmProvider>,
    memory: Arc<EpisodeStore>,
    working_dir: PathBuf,
    inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
    origin: std::sync::Mutex<(String, String)>,
    worker_count: AtomicU32,
    /// Inherited provider policy applied to subagent registries.
    provider_policy: Option<ToolPolicy>,
    /// Optional router for resolving prefixed model IDs to sub-providers.
    provider_router: Option<Arc<ProviderRouter>>,
    /// Default worker prompt for sub-agents (overrides compiled-in worker.txt).
    worker_prompt: Option<String>,
    /// Direct delivery channel to session actor (bypasses InboundMessage relay).
    background_result_sender: Option<BackgroundResultSender>,
    /// Optional lifecycle bridge for durable child-session state.
    child_session_sender: Option<ChildSessionLifecycleSender>,
    /// Inherited lifecycle hooks for spawned workers and background transitions.
    hooks: Option<Arc<HookExecutor>>,
    /// Template used to stamp parent/child session hook context.
    hook_context_template: Option<HookContext>,
    /// Plugin directories to load into subagent registries.
    /// Subagents can use plugin tools (fm_tts, etc.) when listed in allowed_tools.
    plugin_dirs: Vec<PathBuf>,
    /// Extra environment variables for plugin processes.
    plugin_extra_env: Vec<(String, String)>,
    /// Section B (codex review P1.1): inherit the parent's strict-signing
    /// policy so subagents enforce the same integrity gate when loading
    /// plugin tools. Defaults to `false` (legacy permissive path).
    plugin_require_signed: bool,
    /// Additional per-child tools that cannot live in octos-agent builtins.
    child_tool_factories: Vec<ChildToolFactory>,
    /// Shared task supervisor so background subagents show up in task tracking.
    task_supervisor: Option<Arc<TaskSupervisor>>,
    /// Owning session key for tracked background subagents.
    session_key: Option<String>,
    /// Append-only task ledger path for the owning parent session.
    task_ledger_path: Option<PathBuf>,
    /// Optional agent config inherited from the parent session.
    worker_config: Option<AgentConfig>,
    /// Parent's embedding provider, propagated onto worker Agents.
    ///
    /// Without this the spawn path was the only production surface that
    /// both saves episodes (`AgentConfig::default().save_episodes`) and
    /// lacked an embedder — workers stored episodes UNEMBEDDED and their
    /// `build_initial_messages` recall silently skipped (the same NEW-06
    /// class of gap `RunPipelineTool::with_embedder` closed for
    /// pipeline workers).
    embedder: Option<Arc<dyn octos_llm::EmbeddingProvider>>,
    /// Optional MCP-backed sub-agent used when callers pick
    /// `backend == "agent_mcp"`. Parent context stays small because the
    /// sub-agent's internal messages never leak back — only the final
    /// contract-gated artifact flows through [`DispatchResponse`].
    mcp_agent_backend: Option<SharedBackend>,
    /// MCP `tools/call` name dispatched on the backend. Defaults to
    /// [`DEFAULT_MCP_AGENT_TOOL_NAME`].
    mcp_agent_tool_name: Option<String>,
    /// Cost / provenance accountant (M7.4). When present, every
    /// successful MCP sub-agent dispatch writes a
    /// [`crate::cost_ledger::CostAttributionEvent`] to the ledger.
    /// When combined with a budget policy, the dispatcher rejects
    /// spawns whose projected spend breaches the ceiling.
    cost_accountant: Option<Arc<crate::cost_ledger::CostAccountant>>,
    /// M8 Runtime Parity W2.B1: parent session's `FileStateCache` so
    /// spawned child Agents short-circuit re-reads of unchanged files
    /// the same way the parent does. `None` keeps pre-W2 behaviour.
    parent_file_state_cache: Option<Arc<FileStateCache>>,
    /// M8 Runtime Parity W2.B1: parent session's M8.7 output router so
    /// the child Agent's spawn_only background tools route output
    /// through the same on-disk log the dashboard tails.
    parent_subagent_output_router: Option<Arc<SubAgentOutputRouter>>,
    /// Optional callback for live stream-chunk forwarding. When set, the
    /// spawn child's `ProgressEvent::StreamChunk` text is forwarded here so
    /// the WS/serve layer can emit `agent/output/delta` directly (bypassing
    /// the heavy `on_change` + per-token persistence path — per codex review).
    child_stream_callback: Option<ChildStreamCallback>,
    /// M8 Runtime Parity W2.B1: parent session's M8.7 summary generator
    /// so the child can spawn periodic-summary watchers under the same
    /// LLM/budget contract.
    parent_subagent_summary_generator: Option<Arc<AgentSummaryGenerator>>,
    /// Caller-owned context-manager factory for child agents. AppUI/session
    /// runtimes use this to fork the parent context ledger before a subagent
    /// starts, so child prompts are compacted and normalized by the same
    /// durable context path as top-level turns.
    child_prompt_context_manager_factory: Option<ChildPromptContextManagerFactory>,
    /// #714: pre-dispatch policy gate for the `agent_mcp` spawn branch.
    /// Without one, `dispatch_with_metrics` is reached unconditionally —
    /// the same bypass the swarm side closed via
    /// `octos_swarm::SwarmBuilder::with_dispatch_policy` in #710 / #713.
    /// `None` keeps the pre-fix behaviour for callers that opted out
    /// (e.g. legacy tests not exercising the gate).
    dispatch_policy: Option<crate::dispatch_policy::DispatchPolicy>,
    /// #1607 (codex-review follow-up): the session's sandbox config, carried
    /// so the spawn/agent_mcp child completion path can confine `Command`
    /// validators declared by an untrusted workspace `workspace_policy.toml`
    /// to the same backend as the parent's shell/exec tools. Before this
    /// field, the two validator registries in `execute_with_context`'s
    /// `agent_mcp` branch were built with `ToolRegistry::with_builtins`
    /// (hardcoded `NoSandbox`), so `run_project_root_validators` /
    /// `run_declared_validators` executed a workspace-authored `Command`
    /// validator directly on the host even when the session was sandboxed —
    /// a second construction site for the exact escape #1607 closed on the
    /// `build_validator_runner` chokepoint. Defaults to
    /// `SandboxConfig::default()`; the real session wiring threads the same
    /// config the parent `ToolRegistry` was built with via
    /// [`Self::with_sandbox`]. A no-op backend (`NoSandbox`, or a helper that
    /// is unavailable) has nothing to escape, so `ValidatorRunner` runs the
    /// argv directly there — behaviour is unchanged on hosts without a real
    /// backend.
    sandbox: SandboxConfig,
    /// Optional host-owned root for spawned-worker deliverables. When absent,
    /// retain the legacy `<working_dir>/.octos/spawn-deliverables` location.
    deliverable_root: Option<PathBuf>,
    /// Whether Octos itself may create workspace-local state such as a git
    /// worktree. Agent file access is enforced elsewhere; this covers the
    /// host-side control plane so a read-only session cannot create
    /// `.octos/work` before a tool sandbox applies.
    workspace_write_access: bool,
}

impl SpawnTool {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        memory: Arc<EpisodeStore>,
        working_dir: PathBuf,
        inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
    ) -> Self {
        Self {
            llm,
            memory,
            working_dir,
            inbound_tx,
            origin: std::sync::Mutex::new(("cli".into(), "default".into())),
            worker_count: AtomicU32::new(0),
            provider_policy: None,
            provider_router: None,
            worker_prompt: None,
            background_result_sender: None,
            child_session_sender: None,
            hooks: None,
            hook_context_template: None,
            plugin_dirs: Vec::new(),
            plugin_extra_env: Vec::new(),
            plugin_require_signed: false,
            child_tool_factories: Vec::new(),
            task_supervisor: None,
            session_key: None,
            task_ledger_path: None,
            worker_config: None,
            embedder: None,
            mcp_agent_backend: None,
            mcp_agent_tool_name: None,
            cost_accountant: None,
            parent_file_state_cache: None,
            parent_subagent_output_router: None,
            child_stream_callback: None,
            parent_subagent_summary_generator: None,
            child_prompt_context_manager_factory: None,
            dispatch_policy: None,
            sandbox: SandboxConfig::default(),
            deliverable_root: None,
            workspace_write_access: true,
        }
    }

    /// Create a new SpawnTool with context pre-set (for per-session instances).
    pub fn with_context(
        llm: Arc<dyn LlmProvider>,
        memory: Arc<EpisodeStore>,
        working_dir: PathBuf,
        inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
        channel: impl Into<String>,
        chat_id: impl Into<String>,
    ) -> Self {
        Self {
            llm,
            memory,
            working_dir,
            inbound_tx,
            origin: std::sync::Mutex::new((channel.into(), chat_id.into())),
            worker_count: AtomicU32::new(0),
            provider_policy: None,
            provider_router: None,
            worker_prompt: None,
            background_result_sender: None,
            child_session_sender: None,
            hooks: None,
            hook_context_template: None,
            plugin_dirs: Vec::new(),
            plugin_extra_env: Vec::new(),
            plugin_require_signed: false,
            child_tool_factories: Vec::new(),
            task_supervisor: None,
            session_key: None,
            task_ledger_path: None,
            worker_config: None,
            embedder: None,
            mcp_agent_backend: None,
            mcp_agent_tool_name: None,
            cost_accountant: None,
            parent_file_state_cache: None,
            parent_subagent_output_router: None,
            child_stream_callback: None,
            parent_subagent_summary_generator: None,
            child_prompt_context_manager_factory: None,
            dispatch_policy: None,
            sandbox: SandboxConfig::default(),
            deliverable_root: None,
            workspace_write_access: true,
        }
    }

    /// Reconstruct an equivalent `SpawnTool` for a spawned child's own
    /// registry, rebased on the child's working directory.
    ///
    /// Registering the returned tool under the name `spawn` triggers the
    /// [`ToolRegistry::register`] swap that binds `spawn_agent` + `delegate`
    /// behind it, so a subagent can nest a further spawn (bounded by
    /// [`MAX_SPAWN_DEPTH`] via the child worker's incremented
    /// `ToolContext::spawn_depth`). Without this the child registry carries
    /// only the delegate-less builtin `spawn_agent`, and any nested spawn
    /// fails with "No native Octos spawn tool is bound behind spawn_agent in
    /// this ToolRegistry." — orphaning the child task with empty outputs.
    ///
    /// Every wired field (routers, factories, supervisor, sandbox, plugin
    /// dirs, policies, …) is carried forward by `Arc`/value clone so nesting
    /// works identically at each level; `worker_count` restarts at 0 because
    /// each instance numbers only its own direct children, and `origin` is
    /// snapshotted from the parent's current value.
    fn child_spawn_clone(&self, working_dir: PathBuf, child_id: &AgentId) -> SpawnTool {
        SpawnTool {
            llm: self.llm.clone(),
            memory: self.memory.clone(),
            working_dir,
            inbound_tx: self.inbound_tx.clone(),
            origin: std::sync::Mutex::new(
                self.origin
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
            worker_count: AtomicU32::new(0),
            provider_policy: self.provider_policy.clone(),
            provider_router: self.provider_router.clone(),
            worker_prompt: self.worker_prompt.clone(),
            background_result_sender: self.background_result_sender.clone(),
            child_session_sender: self.child_session_sender.clone(),
            hooks: self.hooks.clone(),
            hook_context_template: self.hook_context_template.clone(),
            plugin_dirs: self.plugin_dirs.clone(),
            plugin_extra_env: self.plugin_extra_env.clone(),
            plugin_require_signed: self.plugin_require_signed,
            child_tool_factories: self.child_tool_factories.clone(),
            task_supervisor: self.task_supervisor.clone(),
            session_key: self.session_key.clone(),
            task_ledger_path: self.task_ledger_path.clone(),
            worker_config: self.worker_config.clone(),
            embedder: self.embedder.clone(),
            mcp_agent_backend: self.mcp_agent_backend.clone(),
            mcp_agent_tool_name: self.mcp_agent_tool_name.clone(),
            cost_accountant: self.cost_accountant.clone(),
            parent_file_state_cache: self.parent_file_state_cache.clone(),
            parent_subagent_output_router: self.parent_subagent_output_router.clone(),
            child_stream_callback: self.child_stream_callback.clone(),
            parent_subagent_summary_generator: self.parent_subagent_summary_generator.clone(),
            // Depth-1-only wiring, deliberately DROPPED for grandchildren: the
            // AppUI context-fork factory captures the ORIGINAL parent session's
            // context and keys the fork by worker_id. Carried to a grandchild
            // it would (a) fork from the wrong ancestor and (b) collide keys
            // when worker_count restarts at 0 per level — sync children pass
            // child_session_key=None, so the fallback key is
            // `{base}#spawn-{worker_id}` and `subagent-0`'s grandchild reuses
            // `subagent-0`, overwriting the child's own forked ledger. A
            // grandchild instead runs with a fresh context.
            child_prompt_context_manager_factory: None,
            dispatch_policy: self.dispatch_policy.clone(),
            // NB: `task_supervisor` is carried from the parent here but the
            // caller MUST overwrite it with the child registry's own
            // `tools.supervisor()` before registering, so it matches the
            // `ctx.task_supervisor` a nested `spawn_agent` reads. See the two
            // registration sites in `execute_with_context`.
            sandbox: self.sandbox.clone(),
            // A child starts its direct worker numbering at `subagent-0`, so
            // give nested spawns a distinct descendant root.
            deliverable_root: self
                .deliverable_root
                .as_ref()
                .map(|root| root.join(child_id.to_string()).join("children")),
            workspace_write_access: self.workspace_write_access,
        }
    }

    /// Set a direct result sender that bypasses the InboundMessage relay.
    /// When set, background task results are injected as system messages
    /// into the session without triggering an extra LLM call.
    pub fn with_background_result_sender(mut self, sender: BackgroundResultSender) -> Self {
        self.background_result_sender = Some(sender);
        self
    }

    /// Set a child-session lifecycle sender for background workers.
    pub fn with_child_session_sender(mut self, sender: ChildSessionLifecycleSender) -> Self {
        self.child_session_sender = Some(sender);
        self
    }

    /// Inherit lifecycle hooks from the parent session.
    pub fn with_hooks(mut self, hooks: Arc<HookExecutor>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Set a hook context template for parent/child lifecycle events.
    pub fn with_hook_context(mut self, ctx: HookContext) -> Self {
        self.hook_context_template = Some(ctx);
        self
    }

    /// Inherit a provider-specific tool policy from the parent agent.
    pub fn with_provider_policy(mut self, policy: Option<ToolPolicy>) -> Self {
        self.provider_policy = policy;
        self
    }

    /// #1607 (codex-review follow-up): inherit the session's sandbox config so
    /// the spawn/agent_mcp child completion path confines workspace-declared
    /// `Command` validators to the same backend as the parent's shell/exec
    /// tools. Real session wiring passes the exact `SandboxConfig` the parent
    /// `ToolRegistry` was built with; callers that don't set it keep the
    /// host-independent `SandboxConfig::default()` (which resolves to a no-op
    /// backend when no helper is present, so validators run the argv directly).
    pub fn with_sandbox(mut self, sandbox: SandboxConfig) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Direct host-created deliverables outside the agent workspace. This is
    /// used by read-only chat sessions so Octos bookkeeping cannot create a
    /// `.octos` directory in the reviewed repository.
    pub fn with_deliverable_root(mut self, root: PathBuf) -> Self {
        self.deliverable_root = Some(root);
        self
    }

    /// Control host-side workspace state such as git worktree allocation.
    /// This must track the parent session's file-write capability; it is
    /// separate from the child tool sandbox because allocation happens before
    /// the child registry exists. When disabled, a spawn that requests a
    /// deliverable must also use [`Self::with_deliverable_root`].
    pub fn with_workspace_write_access(mut self, allowed: bool) -> Self {
        self.workspace_write_access = allowed;
        self
    }

    fn deliverable_output_dir(
        &self,
        worker_id: &AgentId,
    ) -> std::result::Result<PathBuf, &'static str> {
        if let Some(root) = &self.deliverable_root {
            return Ok(root.join(worker_id.to_string()));
        }
        if !self.workspace_write_access {
            return Err("read-only spawn requires an external deliverable root");
        }
        Ok(self
            .working_dir
            .join(".octos")
            .join("spawn-deliverables")
            .join(worker_id.to_string()))
    }

    /// Set a provider router for multi-model sub-agent support.
    pub fn with_provider_router(mut self, router: Arc<ProviderRouter>) -> Self {
        self.provider_router = Some(router);
        self
    }

    /// Set a default worker prompt for sub-agents (overrides compiled-in worker.txt).
    pub fn with_worker_prompt(mut self, prompt: String) -> Self {
        self.worker_prompt = Some(prompt);
        self
    }

    /// Set plugin directories and env vars so subagents can use plugin tools.
    pub fn with_plugin_dirs(
        mut self,
        dirs: Vec<PathBuf>,
        extra_env: Vec<(String, String)>,
    ) -> Self {
        self.plugin_dirs = dirs;
        self.plugin_extra_env = extra_env;
        self
    }

    /// Section B (codex review P1.1): inherit the parent's strict-signing
    /// policy. When `true`, subagent plugin loads honour the same
    /// `plugins.require_signed` gate as the parent.
    pub fn with_plugin_require_signed(mut self, require_signed: bool) -> Self {
        self.plugin_require_signed = require_signed;
        self
    }

    /// Add a factory for tools that must be instantiated per child worker.
    pub fn with_child_tool_factory(mut self, factory: ChildToolFactory) -> Self {
        self.child_tool_factories.push(factory);
        self
    }

    /// Register spawned background workers in the shared task supervisor.
    pub fn with_task_supervisor(
        mut self,
        supervisor: Arc<TaskSupervisor>,
        session_key: impl Into<String>,
        task_ledger_path: impl Into<PathBuf>,
    ) -> Self {
        self.task_supervisor = Some(supervisor);
        self.session_key = Some(session_key.into());
        self.task_ledger_path = Some(task_ledger_path.into());
        self
    }

    /// Inherit the parent agent configuration for spawned workers.
    /// Propagate the parent's embedding provider onto every spawned
    /// worker Agent (embed-on-save + hybrid scored/filtered recall).
    pub fn with_embedder(mut self, embedder: Arc<dyn octos_llm::EmbeddingProvider>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Test-only visibility: whether an embedder was threaded through.
    pub fn embedder_for_test(&self) -> Option<&Arc<dyn octos_llm::EmbeddingProvider>> {
        self.embedder.as_ref()
    }

    /// Like [`Self::with_embedder`] but tolerates `None` — call-site
    /// sugar for optional parent embedders.
    pub fn with_optional_embedder(
        mut self,
        embedder: Option<Arc<dyn octos_llm::EmbeddingProvider>>,
    ) -> Self {
        self.embedder = embedder.or(self.embedder);
        self
    }

    pub fn with_agent_config(mut self, config: AgentConfig) -> Self {
        self.worker_config = Some(config);
        self
    }

    /// Configure an MCP-backed sub-agent for this tool instance. Callers
    /// that invoke spawn with `backend: "agent_mcp"` dispatch their task
    /// to `backend` and receive only the final contract-gated artifact in
    /// response — the sub-agent's intermediate messages stay inside the
    /// MCP call.
    pub fn with_mcp_agent_backend(
        mut self,
        backend: SharedBackend,
        tool_name: Option<String>,
    ) -> Self {
        self.mcp_agent_backend = Some(backend);
        self.mcp_agent_tool_name = tool_name;
        self
    }

    /// Convenience: build an MCP-backed sub-agent from typed config and
    /// wire it up as the default backend. The tool's working directory
    /// is forwarded to stdio backends as the child's cwd.
    pub fn with_mcp_agent_backend_config(
        self,
        config: &McpAgentBackendConfig,
        tool_name: Option<String>,
    ) -> Result<Self> {
        let backend = build_backend_from_config(config, Some(self.working_dir.as_path()))?;
        Ok(self.with_mcp_agent_backend(backend, tool_name))
    }

    /// #714: wire a pre-dispatch policy gate for the `agent_mcp` spawn
    /// branch. Mirrors
    /// [`octos_swarm::SwarmBuilder::with_dispatch_policy`] so both
    /// dispatch surfaces fail closed on the same shape of gates
    /// ([`ToolPolicy`], env denylist / allowlist, approval,
    /// `require_sandboxed`). Without one, the agent_mcp branch reaches
    /// [`crate::tools::mcp_agent::dispatch_with_metrics`] directly —
    /// the bypass #714 closes.
    pub fn with_dispatch_policy(mut self, policy: crate::dispatch_policy::DispatchPolicy) -> Self {
        self.dispatch_policy = Some(policy);
        self
    }

    /// Attach a cost / provenance accountant (M7.4). Every successful
    /// MCP sub-agent dispatch routed through this tool records an
    /// attribution on the accountant's ledger. If the accountant carries
    /// a [`crate::cost_ledger::CostBudgetPolicy`], pre-spawn projections
    /// reject dispatches that breach the configured ceiling.
    pub fn with_cost_accountant(
        mut self,
        accountant: Arc<crate::cost_ledger::CostAccountant>,
    ) -> Self {
        self.cost_accountant = Some(accountant);
        self
    }

    /// M8 Runtime Parity W2.B1: inherit the parent session's
    /// `FileStateCache` so spawned child Agents short-circuit re-reads
    /// of unchanged files. Without this, every child re-reads the
    /// entire workspace on every step.
    pub fn with_parent_file_state_cache(mut self, cache: Arc<FileStateCache>) -> Self {
        self.parent_file_state_cache = Some(cache);
        self
    }

    /// M8 Runtime Parity W2.B1: inherit the parent's M8.7 output router
    /// so the child Agent's spawn_only background branch routes output
    /// through the same on-disk log the parent dashboard tails.
    pub fn with_parent_subagent_output_router(mut self, router: Arc<SubAgentOutputRouter>) -> Self {
        self.parent_subagent_output_router = Some(router);
        self
    }

    /// Set a callback that receives live `StreamChunk` text deltas from a
    /// spawned background child. The WS/serve layer uses this to emit
    /// `agent/output/delta` directly — bypassing the per-token `on_change`
    /// persistence path (codex plan review: "per-token persistence/on_change
    /// fan-out is too heavy"). The callback receives
    /// `(agent_id, cursor_offset, text)`:
    /// - `agent_id` is the spawn's `task_id` (the same id surfaced via
    ///   `TurnSpawnCompleteEvent` and the agent dock).
    /// - `cursor_offset` is the cumulative byte offset BEFORE this chunk —
    ///   the START of the delta's window, matching every sibling
    ///   `OutputCursor` producer (`TaskOutputDeltaTracker`, the
    ///   agent_orchestrator read RPCs). Monotonic; lets clients detect
    ///   gaps/reorder on reconnect. `u64` end to end so no lossy cast
    ///   hides between here and `OutputCursor::offset` on the wire.
    /// - `text` is the delta text.
    pub fn with_child_stream_callback(
        mut self,
        cb: impl Fn(&str, u64, &str) + Send + Sync + 'static,
    ) -> Self {
        self.child_stream_callback = Some(Arc::new(cb));
        self
    }

    /// M8 Runtime Parity W2.B1: inherit the parent's M8.7 summary
    /// generator so child agents can drive periodic-summary watchers
    /// under the same LLM/budget contract.
    pub fn with_parent_subagent_summary_generator(
        mut self,
        generator: Arc<AgentSummaryGenerator>,
    ) -> Self {
        self.parent_subagent_summary_generator = Some(generator);
        self
    }

    /// Attach a runtime-owned context manager factory for spawned children.
    pub fn with_child_prompt_context_manager_factory(
        mut self,
        factory: ChildPromptContextManagerFactory,
    ) -> Self {
        self.child_prompt_context_manager_factory = Some(factory);
        self
    }

    /// M8 Runtime Parity W2.B1 introspection helper — used by tests
    /// and the parity audit harness to assert that a SpawnTool was
    /// fully wired with parent caches.
    pub fn parent_file_state_cache(&self) -> Option<&Arc<FileStateCache>> {
        self.parent_file_state_cache.as_ref()
    }

    /// M8 Runtime Parity W2.B1 introspection helper.
    pub fn parent_subagent_output_router(&self) -> Option<&Arc<SubAgentOutputRouter>> {
        self.parent_subagent_output_router.as_ref()
    }

    /// M8 Runtime Parity W2.B1 introspection helper.
    pub fn parent_subagent_summary_generator(&self) -> Option<&Arc<AgentSummaryGenerator>> {
        self.parent_subagent_summary_generator.as_ref()
    }

    /// Dispatch a task to the configured MCP-backed sub-agent. Public so
    /// callers that want direct access (e.g. harness tests) can bypass
    /// the full spawn lifecycle. Returns the raw [`DispatchResponse`]
    /// alongside the typed harness payload the caller should emit.
    pub async fn dispatch_to_mcp_agent(
        &self,
        task: serde_json::Value,
        session_id: &str,
        task_id: &str,
        workflow: Option<&str>,
        phase: Option<&str>,
    ) -> Result<(DispatchResponse, HarnessEvent)> {
        let backend = self
            .mcp_agent_backend
            .as_ref()
            .ok_or_else(|| eyre::eyre!("no MCP agent backend configured on SpawnTool"))?;
        let tool_name = self
            .mcp_agent_tool_name
            .clone()
            .unwrap_or_else(|| DEFAULT_MCP_AGENT_TOOL_NAME.to_string());

        // #714: the public `dispatch_to_mcp_agent` helper is a thin
        // wrapper around `dispatch_with_metrics`. Apply the same policy
        // gate the main `execute` agent_mcp branch uses so a configured
        // policy cannot be bypassed by routing through this helper. The
        // gate inspects the dispatch payload (tool_name + task) before
        // any backend round-trip, so denials never touch the network
        // and never increment the dispatch metric. On denial we
        // synthesise a `RemoteError` response carrying the gate reason
        // so the existing harness-event + dispatch-event pipeline
        // surfaces the failure with the same shape as a backend error.
        if let Some(policy) = self.dispatch_policy.as_ref() {
            if let Err(denial) = crate::dispatch_policy::enforce_dispatch_gates(
                policy,
                backend.as_ref(),
                crate::dispatch_policy::DispatchTarget {
                    dispatch_id: task_id,
                    tool_name: &tool_name,
                    task: &task,
                },
            )
            .await
            {
                warn!(
                    task_id = %task_id,
                    outcome = %denial.last_dispatch_outcome,
                    reason = %denial.reason,
                    "rejecting direct MCP dispatch by DispatchPolicy gate"
                );
                let denied_response = DispatchResponse {
                    outcome: DispatchOutcome::RemoteError,
                    output: String::new(),
                    files_to_send: Vec::new(),
                    error: Some(format!(
                        "dispatch rejected by policy ({}): {}",
                        denial.last_dispatch_outcome, denial.reason
                    )),
                    context_contract: None,
                };
                let payload = build_dispatch_event_payload(
                    session_id,
                    task_id,
                    workflow,
                    phase,
                    backend.as_ref(),
                    &denied_response,
                );
                let event = HarnessEvent {
                    schema: crate::harness_events::HARNESS_EVENT_SCHEMA_V1.to_string(),
                    payload,
                };
                event.validate().map_err(|error| {
                    eyre::eyre!("policy-denied dispatch event failed validation: {error}")
                })?;
                return Ok((denied_response, event));
            }
        }

        let request = DispatchRequest::new(tool_name, task).with_context_contract(
            // #1021 / M17-C — populate backend_kind/agent_id/risk so the
            // evidence ledger can identify this unmanaged dispatch
            // without parsing free-form text. Direct MCP dispatch never
            // forks the Octos prompt context manager, so we tag it
            // `risk: medium` (external transport, no managed context).
            DispatchContextContract::external_unmanaged(
                "direct_mcp_dispatch_has_no_octos_context_manager_payload",
            )
            .with_parent_session_key(Some(session_id.to_string()))
            .with_child_session_key(Some(task_id.to_string()))
            .with_backend_kind("mcp")
            .with_agent_id(task_id.to_string())
            .with_risk("medium"),
        );
        let (response, _summary) = dispatch_with_metrics(backend.as_ref(), request).await;
        let payload = build_dispatch_event_payload(
            session_id,
            task_id,
            workflow,
            phase,
            backend.as_ref(),
            &response,
        );
        let event = HarnessEvent {
            schema: crate::harness_events::HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload,
        };
        event
            .validate()
            .map_err(|error| eyre::eyre!("dispatch event failed validation: {error}"))?;
        Ok((response, event))
    }

    /// Emit a pre-built dispatch event to the given sink. Noop when
    /// `sink_path` is `None` so callers without a supervisor still see
    /// the metrics side-effect without emitting stray events.
    pub fn emit_dispatch_event(sink_path: Option<&str>, event: &HarnessEvent) -> Result<()> {
        let Some(sink) = sink_path else {
            return Ok(());
        };
        write_event_to_sink(sink, event)
            .map_err(|error| eyre::eyre!("failed to write dispatch event to sink: {error}"))
    }

    /// Resolve the LLM provider for a sub-agent based on optional model and context_window.
    ///
    /// Context window priority: LLM-specified > config default > model native.
    fn resolve_sub_provider(
        &self,
        model: Option<&str>,
        context_window: Option<u32>,
    ) -> Result<Arc<dyn LlmProvider>> {
        let (base, default_cw): (Arc<dyn LlmProvider>, Option<u32>) =
            match (model, &self.provider_router) {
                (Some(model_key), Some(router)) => {
                    let provider = router.resolve(model_key)?;
                    // Look up default_context_window from metadata
                    let key = model_key.split_once('/').map_or(model_key, |(k, _)| k);
                    let default_cw = router
                        .list_models_with_meta()
                        .iter()
                        .find(|m| m.key == key)
                        .and_then(|m| m.default_context_window);
                    (provider, default_cw)
                }
                (Some(model_key), None) => {
                    warn!(
                        model = model_key,
                        "model specified but no provider router configured; using parent provider"
                    );
                    (self.llm.clone(), None)
                }
                _ => (self.llm.clone(), None),
            };

        // LLM-specified context_window takes priority, then config default
        let effective_cw = context_window.or(default_cw);
        match effective_cw {
            Some(cw) => Ok(Arc::new(ContextWindowOverride::new(base, cw))),
            None => Ok(base),
        }
    }

    /// Update the origin context for result delivery (called per inbound message).
    pub fn set_context(&self, channel: &str, chat_id: &str) {
        *self.origin.lock().unwrap_or_else(|e| e.into_inner()) =
            (channel.to_string(), chat_id.to_string());
    }
}

/// Upper bound for a spawn's `max_iterations` override. A repo-scale review or
/// deep research legitimately needs well over the interactive default 50
/// one-tool-call-at-a-time steps, but an unbounded value is a runaway-loop /
/// cost footgun, so the caller-supplied value is clamped to this ceiling.
const MAX_SPAWN_MAX_ITERATIONS: u32 = 300;

/// Default iteration budget for a spawned sub-agent when the caller does not set
/// `max_iterations`. The bare `AgentConfig` default (50) is tuned for a snappy
/// *interactive* turn; a background sub-agent does bounded-but-substantive work
/// (a from-scratch repo review runs ~100–150 one-tool-call-at-a-time steps), so
/// blindly inheriting 50 starved real work (agents capped on their first
/// exploration pass, producing nothing). The token budget and loop detection
/// remain the real runaway guards; the iteration cap is a secondary backstop, so
/// a more generous spawn default trades a little worst-case runaway headroom for
/// first-try success on the common substantive-task case. Callers can still
/// raise it up to [`MAX_SPAWN_MAX_ITERATIONS`] or lower it explicitly.
const DEFAULT_SPAWN_MAX_ITERATIONS: u32 = 150;

/// The whole point of the constant above is that it exceeds the interactive
/// default of 50 (`AgentConfig::max_iterations`). Asserted at compile time:
/// both sides are consts, so a runtime `assert!` in a test is really a
/// `clippy::assertions_on_constants` — and this way an edit that breaks the
/// invariant fails the build instead of a test run.
const _: () = assert!(DEFAULT_SPAWN_MAX_ITERATIONS > 50);

/// Resolve the effective iteration budget for a spawn: a caller-supplied value
/// clamped into `[1, MAX]`, or [`DEFAULT_SPAWN_MAX_ITERATIONS`] when unset.
fn resolve_spawn_max_iterations(requested: Option<u32>) -> u32 {
    requested
        .map(|value| value.clamp(1, MAX_SPAWN_MAX_ITERATIONS))
        .unwrap_or(DEFAULT_SPAWN_MAX_ITERATIONS)
}

#[derive(Clone, Deserialize)]
struct Input {
    task: String,
    #[serde(default)]
    label: Option<String>,
    /// "background" (default) or "sync".
    #[serde(default = "default_mode")]
    mode: String,
    /// Worker filesystem isolation strategy. Shared preserves legacy behavior;
    /// worktree gives the child a dedicated git worktree under `.octos/work`.
    #[serde(default)]
    isolation: WorkerIsolation,
    /// Tool names the subagent is allowed to use. Empty = all builtins. A
    /// narrowed list that omits `write_file` leaves the child unable to write a
    /// deliverable except via a shell redirect (which is only captured when a
    /// `deliverable` glob is set) — see the JSON-schema `description`.
    #[serde(default)]
    allowed_tools: Vec<String>,
    /// Extra context injected as a system-level prefix.
    #[serde(default)]
    context: Option<String>,
    /// Prefixed model ID (e.g. "anthropic/claude-haiku") to use a different provider.
    #[serde(default)]
    model: Option<String>,
    /// Override context window size (tokens) for the sub-agent.
    #[serde(default)]
    context_window: Option<u32>,
    /// Override the sub-agent's tool-call iteration budget (default 50).
    /// Clamped to [`MAX_SPAWN_MAX_ITERATIONS`]. Raise it for repo-scale reviews
    /// / research that need many one-tool-call-at-a-time steps.
    #[serde(default)]
    max_iterations: Option<u32>,
    /// Additional instructions appended to the subagent's system prompt.
    /// These are added after the parent's worker prompt, never replacing it.
    #[serde(default, alias = "system_prompt")]
    additional_instructions: Option<String>,
    /// Canonical M14-C backend role template to apply to this child.
    #[serde(default)]
    role: Option<String>,
    /// Optional structured workflow metadata from the session runtime.
    #[serde(default)]
    workflow: Option<WorkflowMetadata>,
    /// Which sub-agent backend services this request. Defaults to
    /// `"builtin"` (in-process [`Agent`]). Set to `"agent_mcp"` to
    /// dispatch via the configured [`super::mcp_agent::McpAgentBackend`].
    #[serde(default = "default_backend")]
    backend: String,
    /// Optional override for the MCP tool name dispatched when
    /// `backend == "agent_mcp"`. Falls back to the SpawnTool's configured
    /// default and finally to [`DEFAULT_MCP_AGENT_TOOL_NAME`].
    #[serde(default)]
    agent_mcp_tool_name: Option<String>,
    /// Optional id of an [`crate::agents::AgentDefinition`] manifest to
    /// resolve from [`crate::tools::ToolContext::agent_definitions`]. When
    /// set, the manifest's fields become defaults for this spawn call;
    /// fields explicitly provided inline on `Input` override the manifest.
    /// Inline always wins.
    #[serde(default)]
    agent_definition_id: Option<String>,
    /// Glob (relative to a dedicated per-task output directory) matching the
    /// deliverable file(s) this spawn is expected to produce. When set, the
    /// child runs in a FRESH output directory seeded with a workspace-contract
    /// artifact declaration and is told to write its deliverable there; any
    /// matching file it leaves — written by ANY means, including a raw `shell`
    /// heredoc that reports no `file_modified` — is surfaced as the task's
    /// `output_files` via the existing workspace-contract artifact resolver.
    /// Empty string is treated as `*` (top-level files). Absent = legacy
    /// behaviour: `output_files` come only from `write_file`/`edit_file` tool
    /// records, so a shell-written deliverable would not surface (issue: the
    /// mini4 code reviews wrote via shell and reported no output_files).
    #[serde(default)]
    deliverable: Option<String>,
}

fn default_backend() -> String {
    "builtin".into()
}

fn default_mode() -> String {
    "background".into()
}

/// Resolve an optional `agent_definition_id` against the context's manifest
/// registry and layer the manifest's fields onto the inline [`Input`].
///
/// Semantics: inline wins. A field already present on `Input` (non-default
/// for `Option`-typed fields; non-empty for `Vec`-typed fields) is kept as-is.
/// Missing fields on `Input` are filled from the manifest.
///
/// Returns an error when the id is set but does not exist in the registry —
/// that's almost always a typo, and silently ignoring it would erase the
/// manifest's safety envelope.
///
/// Returns the manifest's `disallowed_tools` (empty when there is no manifest
/// or it declares none). The caller feeds this to
/// [`build_subagent_tool_policy`] as a **deny-list**, which is the ONLY
/// correct way to enforce it: removing entries from `allowed_tools` (the
/// prior approach) breaks two ways — an allow-list pruned to empty is read by
/// [`ToolPolicy`] as "allow every tool not denied" (a privilege INVERSION),
/// and a one-time prune cannot cover tools a role template adds afterwards
/// (a role could re-introduce a manifest-forbidden tool). A deny entry wins
/// over any allow (including role-provided ones) and never empties the
/// allow-list, so `allow:[shell] + deny:[shell]` → no tools (correct) and
/// `allow:[] + deny:[shell]` → all-except-shell (correct).
fn apply_agent_definition(
    input: &mut Input,
    registry: &crate::agents::AgentDefinitions,
) -> Result<Vec<String>> {
    let Some(id) = input.agent_definition_id.as_deref() else {
        return Ok(Vec::new());
    };
    let def = registry.get(id).ok_or_else(|| {
        eyre::eyre!(
            "spawn: agent_definition_id '{id}' not found in registry; \
             available: [{}]",
            registry.ids().collect::<Vec<_>>().join(", ")
        )
    })?;

    // Tool allow-list: manifest provides the default; inline takes precedence
    // when it is non-empty. The manifest's `disallowed_tools` is NOT applied
    // here — it is returned and enforced as a policy deny-list so it also
    // covers tools a role template contributes later (see the doc comment).
    if input.allowed_tools.is_empty() {
        input.allowed_tools = def.tools.clone();
    }
    let disallowed_tools = def.disallowed_tools.clone();

    // Option-typed fields: manifest only applies when the inline slot is
    // None.
    if input.model.is_none() {
        input.model = def.model.clone();
    }
    // M8.5 fix-first item 5: stop smuggling unsupported `AgentDefinition`
    // fields (`effort`, `permission_mode`) into `additional_instructions`.
    // Hiding them in prompt text gives clients a false sense that the
    // runtime honours the manifest's permission/effort envelope. They
    // remain available on the manifest struct for future enforcement,
    // but they no longer pollute the LLM prompt.
    let _ = def.effort.as_deref();
    let _ = def.permission_mode.as_deref();

    // M8.5 fix-first item 5: reject manifests that set fields the runtime
    // does NOT yet enforce. Today: max_turns, background, memory, hooks,
    // mcp_servers, isolation. Silently accepting them lets clients
    // assume the runtime is honouring envelope state that does nothing,
    // which is exactly the M9 promise the checklist wants to break.
    let unimplemented = def.unimplemented_fields();
    if !unimplemented.is_empty() {
        eyre::bail!(
            "spawn: agent_definition_id '{}' sets unimplemented fields {:?}; \
             remove them from the manifest until the runtime wires them in",
            def.name,
            unimplemented,
        );
    }

    Ok(disallowed_tools)
}

fn append_role_instructions(existing: Option<String>, role_prefix: &str) -> Option<String> {
    if role_prefix.trim().is_empty() {
        return existing;
    }
    Some(match existing {
        Some(existing) if !existing.trim().is_empty() => format!("{role_prefix}\n\n{existing}"),
        _ => role_prefix.to_owned(),
    })
}

fn apply_role_template(input: &mut Input) -> Result<Option<&'static RoleTemplate>> {
    let Some(role) = input
        .role
        .as_deref()
        .map(str::trim)
        .filter(|role| !role.is_empty())
    else {
        return Ok(None);
    };
    let template = RoleTemplate::for_name(role)
        .ok_or_else(|| eyre::eyre!("spawn: unknown role template '{role}'"))?;

    if input.allowed_tools.is_empty() {
        input.allowed_tools = template.allowed_tools_vec();
    }
    input.additional_instructions =
        append_role_instructions(input.additional_instructions.take(), template.prompt_prefix);
    input.role = Some(template.name.to_owned());
    Ok(Some(template))
}

fn should_deliver_output_files(files: &[PathBuf]) -> bool {
    files.iter().any(|path| {
        !matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("md" | "txt" | "json" | "csv")
        )
    })
}

fn encode_workflow_detail(workflow: &WorkflowMetadata) -> Option<String> {
    serde_json::to_string(workflow).ok()
}

/// Coarse progress fraction (0.0–1.0) the workflow_runtime path attaches
/// to `runtime_detail.progress` for a given phase. The runtime owns only
/// two phases per workflow family — the initial phase (`research` /
/// `design` / `scaffold` / etc.) and `deliver_result` after artifacts
/// pass validation — so the curve is deliberately coarse: the runtime
/// stamps a small starting value at spawn and a near-terminal value at
/// the deliver_result transition. Finer-grained values come from the
/// inner tools (e.g. `deep_search` inside `run_pipeline`) emitting
/// `HarnessEvent::progress`, which `task_supervisor::apply_harness_event`
/// folds into the same `runtime_detail.progress` field.
///
/// Without this seed, `runtime_detail.progress` is `null` for the entire
/// initial phase of any workflow whose internal tools do not emit per-event
/// progress, which the e2e live-progress gate spec relies on being non-null.
fn workflow_phase_progress(phase: &str) -> f64 {
    match phase {
        "deliver_result" => 0.95,
        "verify_outputs" | "verify_contract" => 0.9,
        // The initial workflow_runtime phase is family-specific
        // (`research`, `design`, `scaffold`, ...) — treat any non-terminal
        // phase as "just started" so the runtime advertises a non-null
        // progress value rather than `null`.
        _ => 0.05,
    }
}

fn workflow_artifact_matches_kind(path: &Path, kind: &str) -> bool {
    match kind {
        "audio" => matches!(
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .as_deref(),
            Some("mp3" | "wav" | "m4a" | "aac" | "flac" | "ogg")
        ),
        "presentation" => matches!(
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .as_deref(),
            Some("pptx" | "ppt" | "pdf")
        ),
        "site" => matches!(
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .as_deref(),
            Some("html" | "htm" | "xhtml")
        ),
        "report" => matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("md" | "txt" | "pdf" | "html")
        ),
        _ => true,
    }
}

fn workflow_terminal_artifact_kind(workflow: Option<&WorkflowMetadata>) -> Option<&str> {
    workflow?
        .terminal_output
        .as_ref()
        .map(|policy| policy.required_artifact_kind.as_str())
        .filter(|kind| !kind.is_empty())
}

fn task_result_has_terminal_artifact_candidate(
    task_result: &TaskResult,
    workflow: Option<&WorkflowMetadata>,
) -> bool {
    let Some(required_kind) = workflow_terminal_artifact_kind(workflow) else {
        return true;
    };

    task_result
        .files_to_send
        .iter()
        .chain(task_result.files_modified.iter())
        .any(|path| workflow_artifact_matches_kind(path, required_kind))
}

fn select_preferred_terminal_output(
    files: &[PathBuf],
    required_artifact_kind: &str,
) -> Option<PathBuf> {
    files
        .iter()
        .enumerate()
        .max_by_key(|(index, path)| {
            let name = path
                .file_name()
                .and_then(|file| file.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let mut score = 0_i32;
            if name.contains("final") || name.contains("full") {
                score += 20;
            }
            if required_artifact_kind == "audio" {
                if name.contains("podcast") {
                    score += 10;
                }
                if name.ends_with(".mp3") {
                    score += 5;
                }
            } else if required_artifact_kind == "presentation" {
                if name.contains("deck") {
                    score += 10;
                }
                if name.ends_with(".pptx") {
                    score += 5;
                }
            } else if required_artifact_kind == "site" {
                if name.ends_with("index.html") {
                    score += 10;
                }
                if name.contains("site") {
                    score += 5;
                }
            }
            (score, *index as i32)
        })
        .map(|(_, path)| path.clone())
}

fn select_workflow_terminal_files(
    files_to_send: &[PathBuf],
    files_modified: &[PathBuf],
    workflow: Option<&WorkflowMetadata>,
) -> Option<Vec<PathBuf>> {
    let policy = workflow?.terminal_output.as_ref()?;
    let mut candidates = if policy.forbid_intermediate_files {
        let explicit = files_to_send.to_vec();
        if explicit.is_empty() {
            files_modified.to_vec()
        } else {
            explicit
        }
    } else {
        files_to_send
            .iter()
            .chain(files_modified.iter())
            .cloned()
            .collect()
    };

    candidates.retain(|path| workflow_artifact_matches_kind(path, &policy.required_artifact_kind));

    if policy.deliver_final_artifact_only {
        return Some(
            select_preferred_terminal_output(&candidates, &policy.required_artifact_kind)
                .into_iter()
                .collect(),
        );
    }

    Some(candidates)
}

fn workflow_uses_contract_terminal_delivery(workflow: &WorkflowMetadata) -> bool {
    matches!(
        workflow
            .terminal_output
            .as_ref()
            .map(|policy| policy.required_artifact_kind.as_str()),
        Some("presentation" | "site")
    )
}

fn workflow_is_research_podcast(workflow: Option<&WorkflowMetadata>) -> bool {
    workflow.is_some_and(|workflow| workflow.workflow_kind == "research_podcast")
}

fn extract_inline_podcast_script(task_desc: &str) -> Option<String> {
    let header_re = Regex::new(r"\[[^\]\r\n]+?\s+-\s*[^\],\r\n]+,\s*[^\]\r\n]+\]").ok()?;
    let matches = header_re.find_iter(task_desc).collect::<Vec<_>>();
    if matches.len() < 2 {
        return None;
    }

    let mut script_lines = Vec::new();
    for (index, header_match) in matches.iter().enumerate() {
        let text_start = header_match.end();
        let text_end = matches
            .get(index + 1)
            .map(|next| next.start())
            .unwrap_or(task_desc.len());
        let dialogue = task_desc[text_start..text_end].trim();
        if dialogue.is_empty() {
            continue;
        }
        script_lines.push(format!(
            "{} {}",
            header_match.as_str().trim(),
            dialogue.replace('\n', " ").trim()
        ));
    }

    (script_lines.len() >= 2).then(|| script_lines.join("\n"))
}

/// M8 Runtime Parity W2.B2 — single-shot recovery wrapper around
/// `Agent::run_task`. Mirrors the session_actor M8.9 contract:
/// when the first attempt returns either a hard `Err` or a
/// `TaskResult { success: false, .. }`, we synthesize a recovery
/// instruction (using [`build_spawn_recovery_prompt`]) and re-engage
/// the worker exactly once.
///
/// Conservative on purpose:
/// - Only one recovery attempt — second failure bubbles up verbatim.
/// - Reuses the *same* worker / Agent instance so file-state cache,
///   compaction state, and persistent retry buckets are preserved.
/// - The recovery turn is sent as an `additional_instructions`-style
///   tail appended to the original task description, so the worker's
///   conversation history stays linear.
async fn run_task_with_m8_9_recovery(
    worker: &Agent,
    subtask: &Task,
    task_desc: &str,
) -> Result<TaskResult> {
    let initial = worker.run_task(subtask).await;
    let needs_recovery = match &initial {
        Err(_) => true,
        Ok(task_result) => !task_result.success,
    };
    if !needs_recovery {
        return initial;
    }

    let error_message = match &initial {
        Err(error) => format!("{error:#}"),
        Ok(task_result) => {
            // The caller's `output` is the LLM's last assistant message
            // when the worker decided "I cannot continue". Surface that
            // verbatim so the recovery prompt mirrors what the user
            // would see in the chat bubble.
            if task_result.output.trim().is_empty() {
                "task ended unsuccessfully without an explanatory message".to_string()
            } else {
                task_result.output.clone()
            }
        }
    };

    let recovery_prompt = build_spawn_recovery_prompt(task_desc, &error_message);
    let recovery_task = Task::new(
        TaskKind::Code {
            instruction: recovery_prompt,
            files: Vec::new(),
        },
        subtask.context.clone(),
    );
    info!(
        task_id = %subtask.id,
        agent_id = %worker.id,
        "M8.9 spawn-task recovery: re-engaging worker after initial failure"
    );
    worker.run_task(&recovery_task).await
}

/// Build the synthetic `[system-internal]` instruction the spawn-task
/// recovery wrapper sends after a first-pass failure. The shape mirrors
/// `session_actor::build_recovery_prompt` but operates on the
/// pre-LLM task description (we don't have a tool_input here).
fn build_spawn_recovery_prompt(task_desc: &str, error_message: &str) -> String {
    format!(
        "[system-internal] Your previous attempt at the task below failed.\n\
         Original task: {task}\n\
         Failure: {err}\n\n\
         Re-attempt the task. Diagnose the root cause from the failure text, \
         pick a different strategy if appropriate (different tool, different inputs, \
         a smaller scope), and either complete the task or end with a clear \
         explanation of why the task cannot be completed. Do not repeat the same \
         failing step verbatim.",
        task = task_desc,
        err = error_message,
    )
}

async fn maybe_generate_inline_research_podcast(
    tools: &ToolRegistry,
    workflow: Option<&WorkflowMetadata>,
    task_desc: &str,
    task_result: &mut TaskResult,
) {
    if !workflow_is_research_podcast(workflow)
        || !task_result.success
        || task_result_has_terminal_artifact_candidate(task_result, workflow)
    {
        return;
    }

    let Some(script) = extract_inline_podcast_script(task_desc) else {
        return;
    };

    warn!(
        workflow = "research_podcast",
        "worker completed without audio; invoking podcast_generate directly from inline script"
    );
    match tools
        .execute("podcast_generate", &serde_json::json!({ "script": script }))
        .await
    {
        Ok(tool_result) if tool_result.success => {
            if let Some(path) = tool_result.file_modified.clone() {
                task_result.files_modified.push(path);
            }
            task_result
                .files_to_send
                .extend(tool_result.files_to_send.clone());
            let existing = task_result.output.trim();
            task_result.output = if existing.is_empty() {
                tool_result.output
            } else {
                format!("{existing}\n\n{}", tool_result.output)
            };
        }
        Ok(tool_result) => {
            task_result.success = false;
            task_result.output = format!(
                "research_podcast completed without audio, and direct podcast_generate failed: {}",
                tool_result.output
            );
        }
        Err(error) => {
            task_result.success = false;
            task_result.output = format!(
                "research_podcast completed without audio, and direct podcast_generate errored: {error}"
            );
        }
    }
}

/// The EFFECTIVE allow-list for the two consumers that cannot apply the local
/// [`ToolPolicy`] deny-list: `allowed_tools` with every manifest-`disallowed`
/// tool removed.
///
/// - The `agent_mcp` dispatch payload — the REMOTE agent runs its own tool
///   loop and only ever sees this list, so an unfiltered list would grant it a
///   manifest-forbidden tool (the local deny-list never reaches it).
/// - The availability preflight — a manifest-forbidden tool must not gate the
///   spawn on host availability, since the policy denies it regardless.
///
/// The in-process ToolPolicy path deliberately keeps the FULL `allowed_tools`
/// and relies on deny-wins, so this helper must NOT be used to build that
/// policy — doing so would lose the "deny also covers role-refilled tools"
/// guarantee.
fn effective_allowed_tools(allowed_tools: &[String], disallowed_tools: &[String]) -> Vec<String> {
    if disallowed_tools.is_empty() {
        return allowed_tools.to_vec();
    }
    // Deny entries carry the same wildcard (`podcast_*`) and group
    // (`group:runtime`) semantics ToolPolicy enforces locally — prune with a
    // deny-only policy (empty allow = allow everything not denied) so the
    // effective set agrees with what the local policy would actually deny.
    let deny_only = ToolPolicy {
        deny: disallowed_tools.to_vec(),
        ..Default::default()
    };
    allowed_tools
        .iter()
        // Exact-contains AND policy matching: the allow-list may itself carry
        // a group/wildcard entry, and `entry_matches` expands a denied group
        // only against CONCRETE member names (the group string is not a member
        // of itself) — so `group:runtime` denied verbatim would survive pure
        // policy filtering. Contains catches identical entries; the policy
        // catches concrete tools covered by a group/wildcard deny.
        .filter(|tool| !disallowed_tools.contains(tool) && deny_only.is_allowed(tool))
        .cloned()
        .collect()
}

fn build_subagent_tool_policy(
    allowed_tools: Vec<String>,
    disallowed_tools: Vec<String>,
    workflow: Option<&WorkflowMetadata>,
) -> ToolPolicy {
    let mut deny = vec!["spawn".to_string()];
    if workflow.is_some_and(workflow_uses_contract_terminal_delivery) {
        // Contract-owned workflow families must have exactly one runtime-owned
        // terminal delivery path. Deny explicit send_file so child workers
        // cannot double-deliver slides/site artifacts.
        deny.push("send_file".to_string());
    }
    // A manifest's `disallowed_tools` is enforced here as a DENY-list. Deny
    // wins over allow (see `ToolPolicy::evaluate`), so a forbidden tool is
    // blocked even if it appears in `allow` (inline/manifest) OR was added by
    // a role template — and, unlike an allow-list prune, this can never empty
    // the allow-list into an accidental "allow all".
    deny.extend(disallowed_tools);
    ToolPolicy {
        allow: allowed_tools,
        deny,
        ..Default::default()
    }
}

/// Preflight the child's allow-list against what is actually registered.
///
/// `strict` distinguishes who asked for the tools:
/// - `true`  — the CALLER named them explicitly (inline `allowed_tools`). An
///   absent one is a hard error (a typo, or the wrong host): return `Err`.
/// - `false` — a role template or manifest SUGGESTED them (they only fill the
///   allow-list when the caller left it empty). Some are runtime-gated — e.g.
///   the `reviewer` role lists `recall_memory` / `synthesize_research`, which
///   need a memory-store / research provider — so an unwired one must be
///   dropped-with-a-warning, not fail the whole spawn (the mini4 reviewer-role
///   failure: the role required tools that aren't wired on that host).
fn ensure_subagent_tools_available(
    tools: &ToolRegistry,
    allowed_tools: &[String],
    strict: bool,
) -> std::result::Result<(), String> {
    // RFC-0 (#1289): tool deferral was removed — every registered tool is
    // available. Verify the requested CONCRETE tools are present. Skip policy
    // EXPRESSIONS — `group:*` named groups and `*` wildcards — which are
    // allow-list patterns that expand against whatever is registered rather
    // than concrete tool names: `tools.get("group:fs")` is always None, so a
    // caller / role template using group tokens would otherwise be falsely
    // rejected as "not available on this host" (#1689).
    let missing = allowed_tools
        .iter()
        .filter(|entry| !entry.starts_with("group:") && !entry.contains('*'))
        .filter(|tool_name| tools.get(tool_name).is_none())
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else if strict {
        Err(format!(
            "required tool(s) not available on this host: {}",
            missing.join(", ")
        ))
    } else {
        // Role-/manifest-suggested tools that aren't wired here: drop and run
        // with whatever IS available rather than failing the spawn.
        warn!(
            dropped = %missing.join(", "),
            "subagent role/manifest suggested tools not available on this host; dropping them and proceeding"
        );
        Ok(())
    }
}

const PRIMARY_CONTRACT_ARTIFACT: &str = "primary";

fn workflow_contract_kind_label(kind: WorkspaceProjectKind) -> &'static str {
    match kind {
        WorkspaceProjectKind::Slides => "slides",
        WorkspaceProjectKind::Sites => "site",
    }
}

fn workflow_contract_project_kind(workflow: &WorkflowMetadata) -> Option<WorkspaceProjectKind> {
    match workflow
        .terminal_output
        .as_ref()
        .map(|policy| policy.required_artifact_kind.as_str())
    {
        Some("presentation") => Some(WorkspaceProjectKind::Slides),
        Some("site") => Some(WorkspaceProjectKind::Sites),
        _ => None,
    }
}

fn normalize_observed_path(base_dir: &std::path::Path, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn is_matching_workspace_root(path: &std::path::Path, expected_kind: WorkspaceProjectKind) -> bool {
    if !crate::workspace_policy_path(path).is_file() {
        return false;
    }

    matches!(
        (
            expected_kind,
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
        ),
        (WorkspaceProjectKind::Slides, Some("slides"))
            | (WorkspaceProjectKind::Sites, Some("sites"))
    )
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn resolve_contract_workspace_root(
    working_dir: &std::path::Path,
    files_to_send: &[PathBuf],
    files_modified: &[PathBuf],
    workflow: &WorkflowMetadata,
) -> std::result::Result<PathBuf, String> {
    let expected_kind = workflow_contract_project_kind(workflow).ok_or_else(|| {
        "workflow contract root resolution requires a contract-owned artifact kind".to_string()
    })?;
    let kind_label = workflow_contract_kind_label(expected_kind);

    let mut ancestry_candidates = Vec::new();
    for path in files_to_send.iter().chain(files_modified.iter()) {
        let observed = normalize_observed_path(working_dir, path);
        for ancestor in observed.ancestors() {
            if is_matching_workspace_root(ancestor, expected_kind) {
                push_unique_path(&mut ancestry_candidates, ancestor.to_path_buf());
                break;
            }
        }
    }

    match ancestry_candidates.as_slice() {
        [single] => return Ok(single.clone()),
        [] => {}
        _ => {
            return Err(format!(
                "multiple {kind_label} workspace contracts matched observed output paths"
            ));
        }
    }

    if is_matching_workspace_root(working_dir, expected_kind) {
        return Ok(working_dir.to_path_buf());
    }

    let matching_roots = crate::list_workspace_repos(working_dir)
        .map_err(|error| format!("workspace contract discovery failed: {error}"))?
        .into_iter()
        .filter(|repo| repo.kind == expected_kind)
        .map(|repo| repo.root)
        .collect::<Vec<_>>();

    match matching_roots.as_slice() {
        [single] => Ok(single.clone()),
        [] => Err(format!(
            "no {kind_label} workspace contract found beneath {}",
            working_dir.display()
        )),
        _ => Err(format!(
            "multiple {kind_label} workspace contracts found beneath {}; unable to choose a terminal artifact root deterministically",
            working_dir.display()
        )),
    }
}

/// Glob used when a spawn requests deliverable collection with an empty
/// pattern: top-level files only (not `**/*`, which would also sweep up a
/// repository the worker cloned into its output dir).
const DEFAULT_DELIVERABLE_GLOB: &str = "*";

/// Appended to the worker's system prompt when a `deliverable` is declared, so
/// the child writes its output where the seeded contract will find it. Kept
/// tool-agnostic on purpose: the whole point is that a `shell` heredoc into the
/// CWD works just as well as `write_file`.
const DELIVERABLE_WORKER_DIRECTIVE: &str = "DELIVERABLE — IMPORTANT: your job is to WRITE a deliverable FILE into the CURRENT WORKING DIRECTORY, not to explore forever. Do a focused amount of exploration, then WRITE the file EARLY (a solid first draft) and refine it if you have budget left — do NOT run dozens of shell/read commands before writing anything. Anything you leave in the cwd is automatically collected as this task's output; you do NOT need a write_file tool — a shell redirect or heredoc into the cwd (e.g. `cat > report.md <<'EOF'`) works. Do NOT write your deliverable outside the cwd (e.g. not under /tmp). If you finish your run without a written file, your work is LOST — so writing the file is the single most important step, more important than completeness.";

/// A child's final text must be at least this many bytes to be worth
/// salvaging into its declared-but-unwritten deliverable file. Filters out
/// terse "done"/status replies while catching real inline deliveries.
const DELIVERABLE_AUTOMATERIALIZE_MIN_BYTES: usize = 400;

/// Derive a concrete deliverable filename from the declared glob and the
/// task label, for the auto-materialize salvage. The result must MATCH the
/// glob so [`resolve_deliverable_terminal_files`] then surfaces it.
///
/// - single-`*` glob (`*-review.md`, `*.md`, `report-*.txt`) → replace `*`
///   with a slug of the label's first word (`octos-web review` → `octos-web`
///   → `octos-web-review.md`);
/// - literal filename (no `*`) → use it verbatim;
/// - anything else → `<slug>-review.md` (matches the common `*-review.md` /
///   `*.md` review globs).
fn derive_deliverable_filename(glob: &str, label: &str) -> String {
    let slug: String = label
        .split_whitespace()
        .next()
        .unwrap_or("output")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = if slug.trim_matches('-').is_empty() {
        "output".to_string()
    } else {
        slug
    };
    if glob.matches('*').count() == 1 {
        glob.replacen('*', &slug, 1)
    } else if !glob.contains('*') && glob.contains('.') && !glob.contains('/') {
        glob.to_string()
    } else {
        format!("{slug}-review.md")
    }
}

/// Normalize a caller-supplied `deliverable` value into an artifact glob, or
/// `None` when the spawn did not request deliverable collection. An empty
/// string means "collect the top-level files I leave behind".
fn deliverable_artifact_glob(deliverable: Option<&str>) -> Option<String> {
    deliverable.map(|glob| {
        let trimmed = glob.trim();
        if trimmed.is_empty() {
            DEFAULT_DELIVERABLE_GLOB.to_string()
        } else {
            trimmed.to_string()
        }
    })
}

/// Seed a minimal workspace-contract policy in `output_dir` declaring a single
/// `primary` artifact matching `artifact_glob`.
///
/// This reuses the existing workspace-contract harness rather than inventing a
/// bespoke scan: [`resolve_workspace_contract_artifact_paths`] reads this
/// policy's declared glob and resolves matches on disk — the SAME artifact
/// resolver the slides/sites contracts use — so a deliverable written by ANY
/// means (a `shell` heredoc, `write_file`, a plugin) is surfaced, not just
/// files reported through a tracked write tool. The seeded policy is `Session`
/// kind with no validators or git auto-init: we want the artifact-glob
/// resolution, not the slides/sites content validators (which would reject a
/// plain document) or `inspect_workspace_contract_at_root`'s project-kind gate.
fn seed_deliverable_contract(output_dir: &Path, artifact_glob: &str) -> Result<()> {
    use crate::workspace_policy::{
        ValidationPolicy, WorkspaceArtifactsPolicy, WorkspacePolicy, WorkspacePolicyKind,
        WorkspacePolicyWorkspace, WorkspaceSnapshotTrigger, WorkspaceTrackingPolicy,
        WorkspaceVersionControlPolicy, WorkspaceVersionControlProvider, write_workspace_policy,
    };
    use std::collections::BTreeMap;

    std::fs::create_dir_all(output_dir).wrap_err_with(|| {
        format!(
            "failed to create deliverable output dir {}",
            output_dir.display()
        )
    })?;

    let policy = WorkspacePolicy {
        schema_version: crate::abi_schema::WORKSPACE_POLICY_SCHEMA_VERSION,
        workspace: WorkspacePolicyWorkspace {
            kind: WorkspacePolicyKind::Session,
        },
        version_control: WorkspaceVersionControlPolicy {
            provider: WorkspaceVersionControlProvider::Git,
            auto_init: false,
            trigger: WorkspaceSnapshotTrigger::TurnEnd,
            fail_on_error: false,
        },
        tracking: WorkspaceTrackingPolicy { ignore: Vec::new() },
        validation: ValidationPolicy::default(),
        artifacts: WorkspaceArtifactsPolicy {
            entries: BTreeMap::from([(
                PRIMARY_CONTRACT_ARTIFACT.to_string(),
                artifact_glob.to_string(),
            )]),
        },
        spawn_tasks: BTreeMap::new(),
        compaction: None,
    };
    write_workspace_policy(output_dir, &policy)
        .wrap_err("failed to seed deliverable workspace contract")
}

/// Resolve the files a deliverable spawn produced by consulting the seeded
/// workspace contract in `output_dir`. Returns whatever matches the declared
/// `primary` artifact glob on disk — independent of which tool (if any) wrote
/// them. Because the output dir is created fresh per task, no mtime baseline is
/// needed: every match is something the worker left behind this run.
fn resolve_deliverable_terminal_files(output_dir: &Path) -> Vec<PathBuf> {
    resolve_workspace_contract_artifact_paths(output_dir, PRIMARY_CONTRACT_ARTIFACT)
        .unwrap_or_default()
}

fn resolve_background_terminal_files(
    working_dir: &std::path::Path,
    files_to_send: &[PathBuf],
    files_modified: &[PathBuf],
    workflow: Option<&WorkflowMetadata>,
) -> std::result::Result<Vec<PathBuf>, String> {
    if let Some(workflow) =
        workflow.filter(|workflow| workflow_uses_contract_terminal_delivery(workflow))
    {
        let workspace_root =
            resolve_contract_workspace_root(working_dir, files_to_send, files_modified, workflow)?;
        return resolve_contract_terminal_files(&workspace_root, Some(workflow))?
            .ok_or_else(|| "workspace contract returned no terminal files".to_string());
    }

    let terminal_files = select_workflow_terminal_files(files_to_send, files_modified, workflow)
        .unwrap_or_else(|| {
            files_to_send
                .iter()
                .chain(files_modified.iter())
                .cloned()
                .collect()
        });

    if terminal_files.is_empty() {
        if let Some(required_kind) = workflow_terminal_artifact_kind(workflow) {
            let workflow_kind = workflow
                .map(|workflow| workflow.workflow_kind.as_str())
                .unwrap_or("workflow");
            return Err(format!(
                "{workflow_kind} completed without required {required_kind} terminal artifact"
            ));
        }
    }

    Ok(terminal_files)
}

fn format_workspace_contract_failure(status: &WorkspaceContractStatus) -> String {
    let mut failures = Vec::new();
    if let Some(error) = status.error.as_deref() {
        failures.push(error.to_string());
    }
    failures.extend(
        status
            .turn_end_checks
            .iter()
            .chain(status.completion_checks.iter())
            .filter(|check| !check.passed)
            .map(|check| match check.reason.as_deref() {
                Some(reason) if !reason.is_empty() => format!("{}: {}", check.spec, reason),
                _ => format!("{}: failed", check.spec),
            }),
    );
    failures.extend(
        status
            .artifacts
            .iter()
            .filter(|artifact| !artifact.present)
            .map(|artifact| {
                format!(
                    "missing artifact '{}' matching '{}'",
                    artifact.name, artifact.pattern
                )
            }),
    );

    if failures.is_empty() {
        format!("workspace contract for {} is not ready", status.repo_label)
    } else {
        format!(
            "workspace contract for {} is not ready: {}",
            status.repo_label,
            failures.join("; ")
        )
    }
}

fn resolve_contract_terminal_files(
    workspace_root: &std::path::Path,
    workflow: Option<&WorkflowMetadata>,
) -> std::result::Result<Option<Vec<PathBuf>>, String> {
    let Some(workflow) = workflow else {
        return Ok(None);
    };
    if !workflow_uses_contract_terminal_delivery(workflow) {
        return Ok(None);
    }

    let status = crate::inspect_workspace_contract_at_root(workspace_root)
        .map_err(|error| format!("workspace contract inspection failed: {error}"))?;
    if !status.policy_managed {
        return Err(format!(
            "workspace contract missing for {}",
            status.repo_label
        ));
    }
    if !status.ready {
        return Err(format_workspace_contract_failure(&status));
    }

    let terminal_output = workflow
        .terminal_output
        .as_ref()
        .ok_or_else(|| "workflow terminal output policy missing".to_string())?;
    let mut selected = Vec::new();
    let primary_declared = status
        .artifacts
        .iter()
        .any(|artifact| artifact.name == PRIMARY_CONTRACT_ARTIFACT);
    let primary_ready = status
        .artifacts
        .iter()
        .any(|artifact| artifact.name == PRIMARY_CONTRACT_ARTIFACT && artifact.present);

    if terminal_output.deliver_final_artifact_only {
        if !primary_declared {
            return Err(format!(
                "workspace contract for {} is ready but does not declare a '{}' artifact",
                status.repo_label, PRIMARY_CONTRACT_ARTIFACT
            ));
        }

        if !primary_ready {
            return Err(format!(
                "workspace contract for {} is ready but its '{}' artifact is missing",
                status.repo_label, PRIMARY_CONTRACT_ARTIFACT
            ));
        }

        let path = resolve_preferred_workspace_contract_artifact_path(
            workspace_root,
            PRIMARY_CONTRACT_ARTIFACT,
        )
        .map_err(|error| format!("workspace contract resolution failed: {error}"))?;
        return path.map(|path| Some(vec![path])).ok_or_else(|| {
            format!(
                "workspace contract for {} is ready but the '{}' artifact could not be resolved",
                status.repo_label, PRIMARY_CONTRACT_ARTIFACT
            )
        });
    }

    for artifact in status.artifacts.iter().filter(|artifact| artifact.present) {
        selected.extend(
            resolve_workspace_contract_artifact_paths(workspace_root, &artifact.name)
                .map_err(|error| format!("workspace contract resolution failed: {error}"))?,
        );
    }

    selected.sort();
    selected.dedup();

    if !selected.is_empty() {
        return Ok(Some(selected));
    }

    Err(format!(
        "workspace contract for {} is ready but has no resolved artifact paths",
        status.repo_label
    ))
}
async fn deliver_background_result(
    sender: Option<BackgroundResultSender>,
    payload: BackgroundResultPayload,
) -> bool {
    let kind = payload.kind;
    match sender {
        Some(sender) => {
            let delivered = sender(payload).await;
            record_result_delivery(
                "direct_session_actor",
                if delivered { "accepted" } else { "unavailable" },
                kind,
            );
            delivered
        }
        None => {
            record_result_delivery("direct_session_actor", "missing_sender", kind);
            false
        }
    }
}

#[async_trait]
impl Tool for SpawnTool {
    fn name(&self) -> &str {
        "spawn"
    }

    fn description(&self) -> &str {
        "Spawn a subagent to work on a task. Use mode='sync' to wait for the result, or 'background' (default) for fire-and-forget."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        // Item 6 of OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24:
        // spawn() registers a background task with the supervisor,
        // mutates the spawn_only_invoked atomic, and may share the
        // backing memory store with peers in the same batch. Treat it
        // as Exclusive so it never races a sibling tool that also
        // mutates task / session state.
        super::ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> serde_json::Value {
        // Build dynamic model field based on available sub-providers
        let model_prop = match &self.provider_router {
            Some(router) => {
                let models = router.list_models_with_meta();
                if models.is_empty() {
                    serde_json::json!({
                        "type": "string",
                        "description": "Prefixed model ID for the subagent. No sub-providers currently configured."
                    })
                } else {
                    let mut desc_parts =
                        vec!["Model key for the subagent. Available models:".to_string()];
                    let mut enum_vals = Vec::new();
                    for m in &models {
                        let mut line =
                            format!("- '{}': {} ({})", m.key, m.model_id, m.provider_name);
                        if let Some(ref cost) = m.cost_info {
                            line.push_str(&format!(", {cost}"));
                        }
                        line.push_str(&format!(", {}k max ctx", m.context_window / 1000));
                        line.push_str(&format!(", {}k max output", m.max_output_tokens / 1000));
                        if let Some(default_cw) = m.default_context_window {
                            line.push_str(&format!(", {}k default budget", default_cw / 1000));
                        }
                        if let Some(ref desc) = m.description {
                            line.push_str(&format!(". {desc}"));
                        }
                        desc_parts.push(line);
                        enum_vals.push(serde_json::Value::String(m.key.clone()));
                        enum_vals.push(serde_json::Value::String(format!(
                            "{}/{}",
                            m.key, m.model_id
                        )));
                    }
                    serde_json::json!({
                        "type": "string",
                        "description": desc_parts.join("\n"),
                        "enum": enum_vals
                    })
                }
            }
            None => serde_json::json!({
                "type": "string",
                "description": "Prefixed model ID for the subagent (e.g. 'anthropic/claude-haiku'). Requires a provider router."
            }),
        };

        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task for the subagent to complete"
                },
                "label": {
                    "type": "string",
                    "description": "Optional short label for display"
                },
                "mode": {
                    "type": "string",
                    "enum": ["background", "sync"],
                    "description": "background: returns immediately, result announced later. sync: waits and returns the result.",
                    "default": "background"
                },
                "isolation": {
                    "type": "string",
                    "enum": ["shared", "worktree"],
                    "description": "Filesystem isolation for builtin subagents. shared uses the parent workspace; worktree creates a dedicated git worktree under .octos/work.",
                    "default": "shared"
                },
                "allowed_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tool names the subagent may use. Empty = all builtins (the recommended default). CAUTION: if you narrow this list AND the subagent must PRODUCE A FILE (a report, review, generated code, any deliverable), you MUST include `write_file` (and usually `edit_file`) here — OR set the top-level `deliverable` glob. A subagent given `shell` but not `write_file` and no `deliverable` can only write via a shell redirect, and any file it writes outside the working tree (e.g. under /tmp) is LOST (never collected as output_files). When in doubt, leave this empty."
                },
                "role": {
                    "type": "string",
                    "enum": ["reviewer", "implementer", "test_worker", "explorer"],
                    "description": "Backend-owned M14-C role template. When set, the server resolves tool budget, sandbox, approval, model preference, and prompt prefix from the runtime template."
                },
                "context": {
                    "type": "string",
                    "description": "Extra context prepended to the task prompt."
                },
                "model": model_prop,
                "context_window": {
                    "type": "integer",
                    "description": "Override the context window size (tokens) for the subagent."
                },
                "max_iterations": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Override the subagent's tool-call iteration budget (default 50). Raise it for repo-scale reviews or research that need many read/shell steps one at a time; a broad from-scratch review often needs ~100-150. Clamped to a safe ceiling to prevent runaway loops."
                },
                "additional_instructions": {
                    "type": "string",
                    "description": "Extra instructions appended to the subagent's system prompt. Use to specialize behavior (e.g. 'Focus on OWASP Top 10 security issues.'). Cannot override or replace the base system prompt."
                },
                "workflow": {
                    "type": "object",
                    "description": "Optional structured workflow metadata for runtime-owned background workflows.",
                    "properties": {
                        "workflow_kind": {
                            "type": "string",
                            "description": "Stable workflow family identifier."
                        },
                        "current_phase": {
                            "type": "string",
                            "description": "Current workflow phase."
                        },
                        "allowed_tools": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Workflow-owned tool allowlist snapshot."
                        },
                        "terminal_output": {
                            "type": "object",
                            "description": "Runtime-owned final output policy for workflow families.",
                            "properties": {
                                "deliver_final_artifact_only": { "type": "boolean" },
                                "forbid_intermediate_files": { "type": "boolean" },
                                "required_artifact_kind": { "type": "string" }
                            }
                        }
                    },
                    "required": ["workflow_kind", "current_phase"]
                },
                "backend": {
                    "type": "string",
                    "enum": ["builtin", "agent_mcp"],
                    "description": "Sub-agent backend. 'builtin' runs an in-process Agent (default). 'agent_mcp' dispatches to the configured MCP agent backend (Claude Code / Codex / hermes / jiuwenclaw) so the sub-agent's internal tool calls never leak back to the parent context.",
                    "default": "builtin"
                },
                "agent_mcp_tool_name": {
                    "type": "string",
                    "description": "Override the MCP tool name dispatched on the remote agent when backend='agent_mcp'. Defaults to 'run_task'."
                },
                "agent_definition_id": {
                    "type": "string",
                    "description": "Optional id of an AgentDefinition manifest (see crates/octos-agent/src/agents). The manifest's fields (tools, model, max_turns, etc.) become defaults for this spawn; any inline field on the spawn args overrides the manifest (inline wins)."
                },
                "deliverable": {
                    "type": "string",
                    "description": "Glob for the file(s) this spawn should produce (e.g. '*-review.md', 'report.md'). When set, the child runs in a fresh output directory and is told to write its deliverable there; whatever matches the glob is collected as this task's output_files — even if the worker wrote it with a shell heredoc rather than write_file. Empty string means '*' (top-level files). Use a PRECISE glob (avoid '**/*') so a repo the worker clones into its output dir is not swept in."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        // Legacy entry point: route through the typed path with a zero-value
        // context so out-of-band callers behave identically. Manifest-driven
        // spawns require a populated `ctx.agent_definitions`, so legacy
        // callers see a "no such manifest" error if they pass
        // `agent_definition_id` without context — matching the existing
        // guard behaviour for other ctx-dependent fields.
        self.execute_with_context(&super::ToolContext::zero(), args)
            .await
    }

    async fn execute_with_context(
        &self,
        ctx: &super::ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        // Guard C (issue #607): refuse deeply-nested spawn calls before
        // we touch any shared resource (worker counters, supervisor
        // registrations, MCP backends). At `spawn_depth >= MAX_SPAWN_DEPTH`
        // we surface a structured error so the LLM sees a typed failure
        // and the runaway mutual-recursion path collapses.
        if ctx.spawn_depth >= MAX_SPAWN_DEPTH {
            warn!(
                depth = ctx.spawn_depth,
                cap = MAX_SPAWN_DEPTH,
                "spawn refused: depth limit exceeded"
            );
            counter!(
                "octos_spawn_depth_rejected_total",
                "cap" => MAX_SPAWN_DEPTH.to_string()
            )
            .increment(1);
            return Ok(ToolResult {
                output: format!(
                    "Status: FAILED\nspawn depth limit ({MAX_SPAWN_DEPTH}) exceeded; refusing further nesting"
                ),
                success: false,
                ..Default::default()
            });
        }

        // #1690: surface the serde detail AND the schema so the model can
        // self-repair next turn, and return a typed `ToolInputError` — a
        // no-side-effect failure that must NOT cancel well-formed sibling
        // spawns in a serial batch (see the M8.8 cascade in `execution.rs`).
        let mut input: Input = serde_json::from_value(args.clone()).map_err(|e| {
            super::ToolInputError::new(format!(
                "invalid spawn tool input: {e}. Required: `task` (string — the \
                 worker's instructions). Optional: `allowed_tools` (string[], \
                 e.g. [\"group:fs\",\"shell\"]), `role`, `mode` \
                 (\"background\"|\"sync\"), `context`, `model`, `label`."
            ))
        })?;
        // Bug A (reviewer-role preflight): capture whether the CALLER named the
        // tools before a manifest / role template can fill the allow-list (they
        // only fill it when it is empty). A caller-explicit tool that is absent
        // hard-fails the spawn; a role-/manifest-suggested one that is unwired
        // is dropped with a warning (see `ensure_subagent_tools_available`).
        let allow_list_is_caller_explicit = !input.allowed_tools.is_empty();
        // M8.2: if the caller referenced an AgentDefinition manifest by id,
        // layer the manifest's fields onto the inline Input with "inline
        // wins" semantics. Unknown ids are a hard error — silently ignoring
        // them would let a typo erase the manifest's safety envelope.
        // The manifest's `disallowed_tools` becomes a policy DENY-list (below),
        // not an allow-list prune — so it also covers tools a role template
        // adds and can never invert an emptied allow-list into "allow all".
        let manifest_disallowed_tools =
            apply_agent_definition(&mut input, ctx.agent_definitions.as_ref())?;
        let role_template = apply_role_template(&mut input)?;

        let worker_num = self.worker_count.fetch_add(1, Ordering::SeqCst);
        let worker_id = AgentId::new(format!("subagent-{worker_num}"));
        let label = input
            .label
            .unwrap_or_else(|| input.task.chars().take(60).collect());

        // Build the task prompt (optionally prepend context)
        let raw_task = match &input.context {
            Some(ctx) => format!("{ctx}\n\n{}", input.task),
            None => input.task.clone(),
        };
        // #1704: the child's forked context is the parent's whole
        // conversation with this task appended LAST. A weaker model
        // pattern-matches the dominant conversation and answers AS THE
        // PARENT (mini4: a reviewer child's first token was "The user is
        // asking me to check on the status of two agents…" — it ran ls
        // probes and delivered a status table instead of cloning/reviewing).
        // Lead the task with an unambiguous identity + "the history is
        // background, do only this" directive so the actual objective wins.
        let task_desc = frame_subagent_task(&label, &raw_task);

        // An inline list, a manifest (`apply_agent_definition`), and a role
        // template (`apply_role_template`) have each had their chance to fill
        // `allowed_tools`. Empty here means the caller deliberately left the
        // worker unconstrained (every builtin) — honor that; do not synthesize
        // a default surface (tool count is not what makes a worker fail —
        // see the benchmark refutation on #1689).
        let allowed_tools = input.allowed_tools.clone();
        // Effective allow-list = `allowed_tools` minus the manifest's
        // `disallowed_tools`, for the two consumers that cannot apply the local
        // deny-list policy: the `agent_mcp` dispatch payload (codex P1) and the
        // availability preflight (codex P2). See [`effective_allowed_tools`].
        // The local ToolPolicy path below keeps the FULL `allowed_tools` and
        // relies on deny-wins.
        let effective_allowed_tools =
            effective_allowed_tools(&allowed_tools, &manifest_disallowed_tools);
        // #1704 item 3: a read-only role (e.g. "reviewer") handed a
        // clone-and-write task cannot succeed. Warn, and tell the child to
        // deliver findings as text rather than silently failing to produce
        // an impossible artifact.
        if let Some(note) = role_task_capability_warning(&effective_allowed_tools, &input.task) {
            warn!(
                worker = %worker_id,
                role = input.role.as_deref().unwrap_or("<none>"),
                "spawn role/task capability mismatch; injecting deliver-as-text note"
            );
            input.additional_instructions = Some(match input.additional_instructions.take() {
                Some(existing) if !existing.trim().is_empty() => format!("{existing}\n\n{note}"),
                _ => note,
            });
        }
        let workflow = input.workflow.clone();
        let is_sync = input.mode == "sync";
        let is_agent_mcp = input.backend == "agent_mcp";
        if is_agent_mcp && input.isolation == WorkerIsolation::Worktree {
            return Ok(ToolResult {
                output: "Status: FAILED\nworktree isolation is currently supported only for builtin subagents".to_string(),
                success: false,
                ..Default::default()
            });
        }
        // `deliverable` already gives the child a dedicated, scanned output
        // directory. Combined with worktree isolation the child's session scope
        // would be rooted at the worktree while its cwd is the deliverable dir,
        // so the scope would reject the very writes we want to collect. Reject
        // the combination rather than silently drop the deliverable.
        if input.deliverable.is_some() && input.isolation == WorkerIsolation::Worktree {
            return Ok(ToolResult {
                output: "Status: FAILED\n`deliverable` collection is not supported with worktree isolation; it already provides an isolated output directory".to_string(),
                success: false,
                ..Default::default()
            });
        }

        info!(
            worker_id = %worker_id,
            mode = %input.mode,
            backend = %input.backend,
            task = %input.task,
            "spawning subagent"
        );

        // MCP-backed sub-agent dispatch. Runs synchronously (request /
        // response) — the sub-agent's internal tool calls stay inside the
        // MCP call; only the contract-gated artifact flows back. That's
        // the ~10x parent-context saving the M7 plan doc promises.
        if is_agent_mcp {
            let backend = self.mcp_agent_backend.as_ref().ok_or_else(|| {
                eyre::eyre!(
                    "spawn backend='agent_mcp' requires a configured MCP agent backend; \
                     use SpawnTool::with_mcp_agent_backend() to attach one"
                )
            })?;
            let tool_name = input
                .agent_mcp_tool_name
                .clone()
                .or_else(|| self.mcp_agent_tool_name.clone())
                .unwrap_or_else(|| DEFAULT_MCP_AGENT_TOOL_NAME.to_string());
            let session_key_for_event = self
                .session_key
                .clone()
                .unwrap_or_else(|| "sub-agent:unknown-session".to_string());
            let task_id_for_event = worker_id.to_string();
            let workflow_kind = workflow.as_ref().map(|w| w.workflow_kind.clone());
            let workflow_phase = workflow.as_ref().map(|w| w.current_phase.clone());

            let dispatch_payload = serde_json::json!({
                "task": task_desc,
                "label": label,
                "role": input.role,
                // The remote agent runs its own tool loop and never sees the
                // local ToolPolicy, so hand it the already-pruned allow-list
                // (deny-list applied) rather than the raw `allowed_tools`.
                // Also forward `disallowed_tools` so a remote that honors an
                // explicit deny-list can enforce it even if a role template
                // there re-expands the allow-list.
                "allowed_tools": effective_allowed_tools,
                "disallowed_tools": manifest_disallowed_tools,
                "workflow": workflow.clone(),
                "additional_instructions": input.additional_instructions,
            });

            // #714: pre-dispatch policy gate. Runs **before** any
            // budget reservation or backend dispatch so a denial
            // short-circuits the whole pipeline (no reservation taken,
            // no backend touched) — the same ordering the swarm
            // dispatcher uses in `octos_swarm::dispatch_with_budget`.
            // Without a configured policy this is a noop and the
            // existing path is unchanged. With one, the gate enforces
            // `tool_policy`, env denylist / allowlist, `require_approval`,
            // and `require_sandboxed` so the agent_mcp branch can no
            // longer bypass the constraints the operator wired into
            // `octos serve`.
            if let Some(policy) = self.dispatch_policy.as_ref() {
                if let Err(denial) = crate::dispatch_policy::enforce_dispatch_gates(
                    policy,
                    backend.as_ref(),
                    crate::dispatch_policy::DispatchTarget {
                        dispatch_id: &task_id_for_event,
                        tool_name: &tool_name,
                        task: &dispatch_payload,
                    },
                )
                .await
                {
                    warn!(
                        task_id = %task_id_for_event,
                        outcome = %denial.last_dispatch_outcome,
                        reason = %denial.reason,
                        "rejecting MCP sub-agent dispatch by DispatchPolicy gate"
                    );
                    let message = format!(
                        "Status: FAILED\nDispatch rejected by policy ({outcome}): {reason}",
                        outcome = denial.last_dispatch_outcome,
                        reason = denial.reason,
                    );
                    return Ok(ToolResult {
                        output: message,
                        success: false,
                        ..Default::default()
                    });
                }
            }

            // Pre-dispatch budget reservation (F-003). Absent a
            // configured accountant the reservation short-circuits to
            // `None` and the dispatch proceeds unchanged — this keeps
            // existing M7.1 dispatch tests passing when no policy is
            // configured. With a policy, `reserve` closes the TOCTOU
            // race on concurrent dispatches by inserting the projected
            // amount into the accountant's in-memory map under the
            // same lock as the historical-spend read.
            let model_for_ledger = input
                .model
                .clone()
                .unwrap_or_else(|| "unknown-model".to_string());
            let contract_id_for_ledger = workflow_kind
                .clone()
                .unwrap_or_else(|| session_key_for_event.clone());
            let reservation = if let Some(accountant) = self.cost_accountant.as_ref() {
                if accountant.policy().is_some_and(|p| p.is_enforced()) {
                    // Pre-spawn estimate: tokens_in ≈ UTF-8 length of
                    // the outbound task description divided by 4
                    // (the classic 1 token ≈ 4 chars rule of thumb).
                    // Good enough for budget rejection — the ledger
                    // replaces this with the real count on success.
                    let tokens_in_estimate = task_desc.len().div_ceil(4) as u32;
                    let projected_usd = crate::cost_ledger::project_cost_usd(
                        &model_for_ledger,
                        tokens_in_estimate,
                        0,
                    )
                    .unwrap_or(0.0);
                    match accountant
                        .reserve(&contract_id_for_ledger, projected_usd)
                        .await
                    {
                        Ok(handle) => Some(handle),
                        Err(breach) => {
                            let message = format!(
                                "Status: FAILED\nDispatch rejected by cost budget policy: {breach}"
                            );
                            warn!(
                                contract_id = %contract_id_for_ledger,
                                reason = %breach,
                                "rejecting MCP sub-agent dispatch before spawn"
                            );
                            return Ok(ToolResult {
                                output: message,
                                success: false,
                                ..Default::default()
                            });
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            };

            let (response, event) = {
                let request = DispatchRequest::new(tool_name, dispatch_payload)
                    .with_context_contract(
                        // #1021 / M17-C — backend_kind/agent_id/risk
                        // identify this unmanaged dispatch in the
                        // evidence ledger. Same medium-risk tagging as
                        // the direct dispatch path: external MCP
                        // transport that never forks the Octos prompt
                        // context manager.
                        DispatchContextContract::external_unmanaged(
                            "mcp_agent_backend_does_not_consume_octos_prompt_context_manager",
                        )
                        .with_parent_session_key(self.session_key.clone())
                        .with_child_session_key(Some(task_id_for_event.clone()))
                        .with_backend_kind("mcp")
                        .with_agent_id(task_id_for_event.clone())
                        .with_risk("medium"),
                    );
                let (response, _summary) = dispatch_with_metrics(backend.as_ref(), request).await;
                let payload = build_dispatch_event_payload(
                    session_key_for_event.clone(),
                    task_id_for_event.clone(),
                    workflow_kind.as_deref(),
                    workflow_phase.as_deref(),
                    backend.as_ref(),
                    &response,
                );
                let event = HarnessEvent {
                    schema: crate::harness_events::HARNESS_EVENT_SCHEMA_V1.to_string(),
                    payload,
                };
                event.validate().map_err(|error| {
                    eyre::eyre!("sub-agent dispatch event failed validation: {error}")
                })?;
                (response, event)
            };

            if let Some(supervisor) = self.task_supervisor.as_ref() {
                if let Err(error) = supervisor.apply_harness_event(&task_id_for_event, &event) {
                    // The dispatch event is observational; absence of a
                    // tracked task is not a dispatch failure. Log and
                    // continue.
                    warn!(
                        task_id = %task_id_for_event,
                        error = %error,
                        "dispatch event could not be applied to task supervisor"
                    );
                }
            }

            let success = response.outcome == super::mcp_agent::DispatchOutcome::Success;

            // Post-dispatch cost attribution (M7.4 + F-003). Only
            // record when the remote agent returned a ready artifact;
            // failures and timeouts are already visible via the
            // dispatch event and should not inflate the ledger. On the
            // failure path the reservation handle is dropped below,
            // auto-refunding the pre-dispatch projection.
            if success {
                if let Some(accountant) = self.cost_accountant.as_ref() {
                    let tokens_in_est = task_desc.len().div_ceil(4) as u32;
                    let tokens_out_est = response.output.len().div_ceil(4) as u32;
                    let cost_usd = crate::cost_ledger::project_cost_usd(
                        &model_for_ledger,
                        tokens_in_est,
                        tokens_out_est,
                    )
                    .unwrap_or(0.0);
                    let attribution = crate::cost_ledger::CostAttributionEvent::new(
                        session_key_for_event.clone(),
                        contract_id_for_ledger.clone(),
                        task_id_for_event.clone(),
                        model_for_ledger.clone(),
                        tokens_in_est,
                        tokens_out_est,
                        cost_usd,
                    )
                    .with_workflow(workflow_kind.clone(), workflow_phase.clone())
                    .with_backend_outcome(
                        Some(backend.as_ref().backend_label().to_string()),
                        Some("success".to_string()),
                    );
                    let attribution_id_for_event = attribution.attribution_id.clone();

                    // Commit through the reservation handle if we hold
                    // one (policy-enforced path). Otherwise fall back
                    // to the legacy direct-record path for the
                    // no-policy configuration. Failure to persist is
                    // non-fatal — we log and continue so a bad disk
                    // does not mask a successful agent run.
                    let record_result = if let Some(handle) = reservation.as_ref() {
                        handle.commit(attribution).await
                    } else {
                        accountant.ledger().record(attribution).await
                    };

                    if let Err(error) = record_result {
                        warn!(
                            task_id = %task_id_for_event,
                            error = %error,
                            "failed to persist cost attribution; dispatch succeeded"
                        );
                    } else {
                        // Emit the typed event so downstream sinks,
                        // including the operator summary aggregator,
                        // see the spend even without re-reading the
                        // ledger.
                        let cost_event = HarnessEvent::cost_attribution(
                            crate::harness_events::HarnessCostAttributionEvent {
                                schema_version: crate::abi_schema::COST_ATTRIBUTION_SCHEMA_VERSION,
                                session_id: session_key_for_event.clone(),
                                task_id: task_id_for_event.clone(),
                                workflow: workflow_kind.clone(),
                                phase: workflow_phase.clone(),
                                attribution_id: attribution_id_for_event,
                                contract_id: contract_id_for_ledger.clone(),
                                model: model_for_ledger.clone(),
                                tokens_in: tokens_in_est,
                                tokens_out: tokens_out_est,
                                cost_usd,
                                outcome: "success".to_string(),
                                extra: std::collections::HashMap::new(),
                            },
                        );
                        if let Err(error) = cost_event.validate() {
                            warn!(
                                task_id = %task_id_for_event,
                                error = %error,
                                "cost attribution event failed validation; skipping emission"
                            );
                        } else if let Some(supervisor) = self.task_supervisor.as_ref() {
                            if let Err(error) =
                                supervisor.apply_harness_event(&task_id_for_event, &cost_event)
                            {
                                warn!(
                                    task_id = %task_id_for_event,
                                    error = %error,
                                    "cost attribution event could not be applied"
                                );
                            }
                        }
                    }
                }
            }
            // On the failure path, drop the reservation explicitly so
            // the auto-refund fires before we return the `Status: FAILED`
            // result. The handle is scoped to this block — either
            // `commit` above consumed it successfully, or Drop refunds.
            drop(reservation);

            // Review A F-004: for the agent_mcp dispatch path the child
            // session runs inside the remote backend and never touches the
            // parent's ValidatorRunner. Before, the parent trusted the
            // remote `SUCCESS` label — if the remote skipped its own
            // contract-gate, the parent happily forwarded a non-validated
            // artifact. Running the declared completion-phase validators
            // here, against the parent's workspace root, restores the
            // invariant: any required validator failure demotes the
            // response to a typed failure before it leaves the tool.
            //
            // octos #997 (round-4 fix): run both the session-scope and
            // project-scope validator blocks BEFORE
            // `resolve_contract_terminal_files`. With
            // `terminal_output.required_artifact_kind = "presentation"`
            // (real `slides_delivery` shape),
            // `resolve_contract_terminal_files` calls
            // `inspect_workspace_contract_at_root` which reads the project
            // ledger at
            // `<session>/<kind>/<slug>/.octos/validator_outcomes.jsonl`.
            // If validators run AFTER that gate, the gate returns
            // `ready = false` (empty ledger) and the agent_mcp branch
            // early-returns at `Err(error) => return Ok(...)` before
            // either validator block executes. Re-ordering ensures the
            // project ledger is populated first, so the contract gate
            // inside `resolve_contract_terminal_files` sees the real
            // `Pass` rows.
            let mut mcp_success = success;
            let mut mcp_output_override: Option<String> = None;
            if mcp_success {
                if let Ok(Some(policy)) =
                    crate::workspace_policy::read_workspace_policy(&self.working_dir)
                {
                    if !policy.validation.validators.is_empty() {
                        // #1607 (codex-review follow-up): build this
                        // session-scope validator registry with the session's
                        // sandbox so a workspace-authored `Command` validator
                        // runs confined, not directly on the host. `with_builtins`
                        // hardcodes `NoSandbox`, so `tools.sandbox()` inside
                        // `build_validator_runner` would be a no-op here even
                        // when the parent session is sandboxed. A no-op backend
                        // (no helper present) still runs the argv directly, so
                        // hosts without a real sandbox are unaffected.
                        let validator_sandbox: std::sync::Arc<dyn crate::sandbox::Sandbox> =
                            std::sync::Arc::from(create_sandbox(&self.sandbox));
                        let mut registry_for_validators = ToolRegistry::with_builtins_and_sandbox(
                            &self.working_dir,
                            create_sandbox(&self.sandbox),
                        );
                        // Honour the parent's provider tool policy in the validator
                        // registry too, so a workspace `ToolCall` validator can't
                        // invoke a tool the policy denies (#1607 codex round 2).
                        if let Some(policy) = self.provider_policy.clone() {
                            registry_for_validators.set_provider_policy(policy);
                        }
                        if let Err(reason) = crate::workspace_contract::run_declared_validators(
                            &registry_for_validators,
                            &self.working_dir,
                            &policy.validation.validators,
                            "spawn-agent-mcp",
                            crate::validators::ValidatorPhase::Completion,
                            None,
                            validator_sandbox,
                        )
                        .await
                        {
                            mcp_success = false;
                            mcp_output_override = Some(format!(
                                "Status: FAILED\nremote_agent_mcp: completion validator rejected child artifact: {reason}"
                            ));
                        }
                    }
                }
            }

            // octos #997 (round-3 fix): the session-scope validator block above
            // runs against `self.working_dir` (the session root) and writes the
            // session ledger only. The project-scope contract gate
            // (`inspect_workspace_contract`) reads
            // `<session>/<kind>/<slug>/.octos/validator_outcomes.jsonl`. Without
            // this run, an `agent_mcp` slides dispatch that produces a valid
            // PPTX would leave the project ledger empty and a downstream
            // contract gate would surface `ready = false`. Mirror the sync
            // (`:2312`) and background (`:2680`) spawn fixes so the agent_mcp
            // branch closes the same bypass.
            if mcp_success {
                let expected_kind = workflow.as_ref().and_then(workflow_contract_project_kind);
                // #1607 (codex-review follow-up): same rationale as the
                // session-scope block above — the project-root validator pass
                // runs `Command` validators declared by an untrusted
                // `slides/<slug>` or `sites/<slug>` `workspace_policy.toml`, so
                // it MUST inherit the session sandbox instead of `with_builtins`'
                // hardcoded `NoSandbox`. This is the child mirror of the
                // `build_validator_runner` chokepoint fix; without it the
                // agent_mcp branch is a second unsandboxed construction site.
                let validator_sandbox: std::sync::Arc<dyn crate::sandbox::Sandbox> =
                    std::sync::Arc::from(create_sandbox(&self.sandbox));
                let mut registry_for_validators = ToolRegistry::with_builtins_and_sandbox(
                    &self.working_dir,
                    create_sandbox(&self.sandbox),
                );
                // Honour the parent's provider tool policy in the validator
                // registry too, so a workspace `ToolCall` validator can't invoke
                // a tool the policy denies (#1607 codex round 2).
                if let Some(policy) = self.provider_policy.clone() {
                    registry_for_validators.set_provider_policy(policy);
                }
                let report = crate::workspace_contract::run_project_root_validators(
                    &registry_for_validators,
                    &self.working_dir,
                    expected_kind,
                    &response.files_to_send,
                    validator_sandbox,
                )
                .await;
                if let Some(reason) = report.first_failure_reason() {
                    mcp_success = false;
                    mcp_output_override = Some(format!(
                        "Status: FAILED\nremote_agent_mcp: project-scope validator rejected child artifact: {reason}"
                    ));
                }
            }

            // Workflow contract families always gate outputs through the
            // workspace contract. The dispatch response is advisory; the
            // final delivery path remains owned by the runtime.
            //
            // Runs LAST so the validator blocks above have already written
            // the session + project ledgers; `inspect_workspace_contract_at_root`
            // (inside `resolve_contract_terminal_files`) reads those ledgers
            // to decide `ready`. Skipped on validator failure — empty
            // `files_to_send` is correct for a failed result.
            let mut files_to_send = response.files_to_send.clone();
            if mcp_success {
                if let Some(workflow_meta) = workflow.as_ref() {
                    if workflow_uses_contract_terminal_delivery(workflow_meta) {
                        match resolve_contract_terminal_files(
                            self.working_dir.as_path(),
                            Some(workflow_meta),
                        ) {
                            Ok(Some(contract_files)) => files_to_send = contract_files,
                            Ok(None) => {}
                            Err(error) => {
                                return Ok(ToolResult {
                                    output: format!("Status: FAILED\n{error}"),
                                    success: false,
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }

            return Ok(ToolResult {
                output: mcp_output_override.unwrap_or_else(|| {
                    if mcp_success {
                        format!("Status: SUCCESS\n\n{}", response.output)
                    } else {
                        format!(
                            "Status: FAILED\n{}",
                            response
                                .error
                                .clone()
                                .unwrap_or_else(|| response.output.clone())
                        )
                    }
                }),
                success: mcp_success,
                files_to_send: if mcp_success {
                    files_to_send
                } else {
                    Vec::new()
                },
                ..Default::default()
            });
        }

        // PR #1250 review, findings 1+2: `allocate_worker_worktree` validates
        // the planned path — canonical `.octos/work` containment under the
        // repo root, plus `SessionScope::with_workspace` when a scope is
        // present — BEFORE `git worktree add`, and returns the allocation
        // armed inside an RAII guard. Any refusal or `?` return between here
        // and the worker handoff drops the guard, which prunes the worktree
        // and its branch; the guard is disarmed at the two handoff points
        // (right before the sync task run / right before the background
        // `tokio::spawn`).
        //
        // Only worktree isolation rebinds the session scope. A shared spawn
        // runs in the parent's workspace, so rebasing the scope onto
        // `self.working_dir` — which in gateway/session-actor wiring is the
        // factory cwd, outside the per-profile scope root — would wrongly
        // fail the default (shared) spawn. Shared keeps the parent scope.
        let (mut worker_worktree_guard, child_session_scope) = match input.isolation {
            WorkerIsolation::Shared => (None, ctx.session_scope.clone()),
            WorkerIsolation::Worktree => {
                if !self.workspace_write_access {
                    return Ok(ToolResult {
                        output: "Status: FAILED\nworktree isolation is unavailable in a read-only workspace; use shared isolation for a review-only worker".to_string(),
                        success: false,
                        ..Default::default()
                    });
                }
                match allocate_worker_worktree(
                    &self.working_dir,
                    &worker_id,
                    ctx.session_scope.as_ref(),
                ) {
                    Ok((guard, scope)) => (Some(guard), scope),
                    Err(error) => {
                        return Ok(ToolResult {
                            output: format!(
                                "Status: FAILED\nfailed to allocate worker git worktree: {error}"
                            ),
                            success: false,
                            ..Default::default()
                        });
                    }
                }
            }
        };
        // Deliverable contract (option 2): when a spawn declares a
        // `deliverable` glob, give the child a FRESH per-task output directory
        // seeded with a workspace-contract artifact declaration, tell it to
        // write its deliverable into that directory, and surface whatever
        // matches on completion — even a raw `shell` heredoc that reports no
        // `file_modified`. This reuses the workspace-contract artifact resolver
        // (`resolve_workspace_contract_artifact_paths`) rather than the
        // tool-record-only path, which is blind to shell writes.
        let deliverable_glob = deliverable_artifact_glob(input.deliverable.as_deref());
        if deliverable_glob.is_some() {
            input.additional_instructions = Some(match input.additional_instructions.take() {
                Some(existing) if !existing.trim().is_empty() => {
                    format!("{existing}\n\n{DELIVERABLE_WORKER_DIRECTIVE}")
                }
                _ => DELIVERABLE_WORKER_DIRECTIVE.to_string(),
            });
        }
        let child_working_dir = match deliverable_glob.as_deref() {
            Some(glob) => {
                let out = match self.deliverable_output_dir(&worker_id) {
                    Ok(out) => out,
                    Err(error) => {
                        return Ok(ToolResult {
                            output: format!("Status: FAILED\n{error}"),
                            success: false,
                            ..Default::default()
                        });
                    }
                };
                // worker_id ("subagent-N") can repeat across turns, so start
                // from a clean directory — otherwise a stale deliverable from a
                // prior run would be surfaced (the output dir has no mtime
                // baseline because it is meant to be fresh).
                let _ = std::fs::remove_dir_all(&out);
                if let Err(error) = seed_deliverable_contract(&out, glob) {
                    return Ok(ToolResult {
                        output: format!(
                            "Status: FAILED\nfailed to seed deliverable output dir: {error}"
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
                out
            }
            None => worker_worktree_guard
                .as_ref()
                .map(|guard| guard.worktree().path.clone())
                .unwrap_or_else(|| self.working_dir.clone()),
        };

        let sub_llm = self.resolve_sub_provider(input.model.as_deref(), input.context_window)?;

        // Review A F-004: snapshot the parent workspace policy once so both the
        // sync and async spawn branches propagate the same typed
        // compaction / validator contracts to child sessions. Without this,
        // the child Agent silently runs without preflight compaction even
        // when the parent's workspace_policy.toml declares one.
        let parent_workspace_policy =
            match crate::workspace_policy::read_workspace_policy(&self.working_dir) {
                Ok(policy) => policy,
                Err(error) => {
                    warn!(
                        working_dir = %self.working_dir.display(),
                        error = %error,
                        "spawn: failed to read parent workspace policy; \
                         child will run without propagated compaction/validator contracts"
                    );
                    None
                }
            };

        if is_sync {
            // Sync mode: run subagent inline and return the result directly
            //
            // #1607 (codex-review follow-up): build the child registry with the
            // SESSION sandbox, not the hardcoded `NoSandbox` that
            // `with_builtins` stores. The child's `child_tools_handle` feeds
            // `run_declared_validators` / `run_project_root_validators` below,
            // and `build_validator_runner` confines `ValidatorSpec::Command`
            // validators to `tools.sandbox()`. A `NoSandbox` registry there
            // would let an untrusted `workspace_policy.toml` command validator
            // execute directly on the host from a sandboxed session. On hosts
            // without a real backend `create_sandbox` yields `NoSandbox` and
            // the validator runs the argv directly (unchanged).
            let mut tools = ToolRegistry::with_builtins_and_sandbox(
                &child_working_dir,
                create_sandbox(&self.sandbox),
            );
            // Load plugin tools so subagents can use fm_tts, etc.
            // Section B (codex review P1.1): honour the parent's
            // require_signed policy so unsigned plugins are rejected here
            // when strict mode is on.
            if !self.plugin_dirs.is_empty() {
                let _ = crate::plugins::PluginLoader::load_into_with_options(
                    &mut tools,
                    &self.plugin_dirs,
                    &self.plugin_extra_env,
                    crate::plugins::PluginLoadOptions {
                        work_dir: Some(&child_working_dir),
                        synthesis_config: None,
                        require_signed: self.plugin_require_signed,
                        verified_cache_dir: None,
                    },
                );
                // SPEC-VENDOR-NODE-V1 HTTP tool discovery — hard-fail per
                // @ymote's Finding 2 contract. Subagents need the bridge tools
                // the parent has; if discovery fails the spawn must error out
                // rather than silently spawn a tool-blind subagent.
                crate::plugins::register_http_skills_on_startup(&mut tools, &self.plugin_dirs)
                    .await
                    .map_err(|e| eyre::eyre!("subagent HTTP tool discovery failed: {e}"))?;
            }
            for factory in &self.child_tool_factories {
                tools.register_arc(factory());
            }
            // Bind the child's OWN native `spawn` delegate. Registering a
            // tool named "spawn" triggers the `ToolRegistry` swap that binds
            // `spawn_agent` + `delegate` behind it, so a subagent can nest a
            // further spawn instead of hitting the delegate-less builtin
            // ("No native Octos spawn tool is bound…"). `MAX_SPAWN_DEPTH`
            // (via the child worker's incremented `ctx.spawn_depth`) bounds
            // the recursion, and the subagent policy still denies DIRECT
            // `spawn` (deny is exact-match, so `spawn_agent` stays allowed).
            //
            // The spawn tool and the registry it lives in MUST share one
            // supervisor (the top level wires `with_task_supervisor(
            // tool_registry.supervisor())`). Bind the clone to THIS child
            // registry's OWN supervisor — the one `ctx.task_supervisor`
            // exposes — so `SpawnAgentTool`'s before/after task lookup finds
            // the task the delegate registers; otherwise `spawn_agent` returns
            // no `agent_id` and `delegate` fails with "did not register a task".
            //
            // This keeps each spawned subtree's nested tasks in its OWN
            // (private, per-child) supervisor rather than the shared session
            // one — a deliberate isolation choice. Sharing the session
            // supervisor would (a) let `newest_spawned_task` mis-correlate
            // across CONCURRENT sibling spawners racing the same map,
            // (b) give the child's task-control aliases (wait/close/resume)
            // session-wide reach, and (c) leak a child's own `run_pipeline`
            // node tasks into the persisted session ledger. The trade-off:
            // grandchildren are not surfaced in the session task/list and the
            // fan-out cap is per-subtree, not global. The grandchild's RESULT
            // still flows back via the inherited result sender / inline
            // output; only its task-tracking row stays subtree-local.
            let mut child_spawn = self.child_spawn_clone(child_working_dir.clone(), &worker_id);
            child_spawn.task_supervisor = Some(tools.supervisor());
            tools.register(child_spawn);
            // In subagent context, spawn_only tools should be regular tools —
            // the subagent IS the background, so no need to auto-background again.
            tools.clear_spawn_only();
            // RFC-1 fixup (codex P1): also clear internal-hidden markers.
            // In a subagent registry, the mofa_make dispatcher's targets
            // (mofa_slides, mofa_cards, …) should be directly callable —
            // the subagent's whole purpose may be to drive that target
            // tool; routing through a dispatcher adds latency without
            // value once we are already in a spawned context.
            tools.clear_internal_hidden();
            // Preflight against the EFFECTIVE (post-deny) allow-list: a tool
            // the manifest forbids must not gate the spawn on availability,
            // since the policy below denies it regardless (codex P2).
            ensure_subagent_tools_available(
                &tools,
                &effective_allowed_tools,
                allow_list_is_caller_explicit,
            )
            .map_err(|error| eyre::eyre!(error))?;
            let policy = build_subagent_tool_policy(
                allowed_tools,
                manifest_disallowed_tools,
                workflow.as_ref(),
            );
            tools.apply_policy(&policy);
            if let Some(ref pp) = self.provider_policy {
                tools.set_provider_policy(pp.clone());
            }
            let mut worker = Agent::new(worker_id, sub_llm.clone(), tools, self.memory.clone())
                // Guard C (issue #607): stamp the child agent's spawn
                // nesting depth as `parent_depth + 1` so the child's
                // own spawn tool calls see the higher value and the
                // [`MAX_SPAWN_DEPTH`] gate fires at the bounded limit.
                .with_spawn_depth(ctx.spawn_depth.saturating_add(1));
            // Phase 2-D of the SessionScope migration: propagate the
            // parent's scope into the child Agent so the child's tools
            // (shell, read_file, write_file, edit_file, plugin tool,
            // pipeline workers) all see the same filesystem contract.
            // Worktree-isolated children inherit the same root/mode policy
            // with their workspace rebound to the worker worktree.
            if let Some(scope) = child_session_scope.as_ref() {
                worker = worker.with_session_scope(scope.clone());
            }
            // Keep an Arc handle to the child's tool registry so we can run
            // declared validators against it after `run_task` returns.
            let child_tools_handle = worker.tool_registry().clone();
            // Apply the worker config plus the spawn's iteration budget: the
            // caller's `max_iterations` (clamped) or the generous spawn default
            // (`DEFAULT_SPAWN_MAX_ITERATIONS`) — a sub-agent does more than an
            // interactive turn, so it must not inherit the bare 50 default.
            {
                let mut config = self.worker_config.clone().unwrap_or_default();
                config.max_iterations = resolve_spawn_max_iterations(input.max_iterations);
                worker = worker.with_config(config);
            }
            if let Some(factory) = self.child_prompt_context_manager_factory.as_ref() {
                if let Some(manager) = factory(ChildPromptContextRequest {
                    parent_session_key: self.session_key.clone(),
                    child_session_key: None,
                    task_id: None,
                    worker_id: worker.id.to_string(),
                    task_label: label.clone(),
                }) {
                    worker = worker.with_prompt_context_manager(manager);
                }
            }

            // M8 Runtime Parity W2.B1: inherit parent caches so the child
            // observes the same file_state_cache + subagent_output_router
            // + subagent_summary_generator the session actor wired. This
            // closes the gap where spawned subagents had `file_state_cache:
            // None` and re-read the entire workspace on every step.
            if let Some(ref cache) = self.parent_file_state_cache {
                worker = worker.with_file_state_cache(cache.clone());
            }
            if let Some(ref router) = self.parent_subagent_output_router {
                worker = worker.with_subagent_output_router(router.clone());
            }
            if let Some(ref summary_gen) = self.parent_subagent_summary_generator {
                worker = worker.with_subagent_summary_generator(summary_gen.clone());
            }
            // Embed-on-save + recall parity: workers save episodes by
            // default, so without the parent's embedder those episodes
            // are stored vectorless and worker recall skips entirely.
            if let Some(ref embedder) = self.embedder {
                worker = worker.with_embedder(embedder.clone());
            }

            // Review A F-004: propagate the parent's declarative compaction
            // policy onto the child Agent so the child honours the same token
            // budget and preserved-artifact contract the parent committed to.
            if let Some(ref policy) = parent_workspace_policy {
                if let Some(compaction_policy) = policy.compaction.clone() {
                    let runner = match compaction_policy.summarizer {
                        crate::workspace_policy::CompactionSummarizerKind::LlmIterative => {
                            crate::compaction::CompactionRunner::with_provider(
                                compaction_policy,
                                sub_llm.clone(),
                            )
                        }
                        crate::workspace_policy::CompactionSummarizerKind::Extractive => {
                            crate::compaction::CompactionRunner::new(compaction_policy)
                        }
                    }
                    .with_workspace_policy(policy);
                    worker = worker
                        .with_compaction_runner(Arc::new(runner))
                        .with_compaction_workspace(policy.clone());
                }
            }

            // Base prompt: configured worker prompt, or compiled-in default.
            // Additional instructions are appended, never replacing the base.
            let base_prompt = self
                .worker_prompt
                .clone()
                .unwrap_or_else(|| crate::DEFAULT_WORKER_PROMPT.to_string());
            let full_prompt = match &input.additional_instructions {
                Some(extra) if !extra.is_empty() => format!("{base_prompt}\n\n{extra}"),
                _ => base_prompt,
            };
            worker = worker.with_system_prompt(full_prompt);

            let subtask = Task::new(
                TaskKind::Code {
                    instruction: task_desc.clone(),
                    files: vec![],
                },
                TaskContext {
                    working_dir: child_working_dir.clone(),
                    ..Default::default()
                },
            );

            // PR #1250 finding 1: every refusal point is behind us — the
            // worker starts now. Disarm the prune guard; from here the
            // worktree's lifecycle is owned by its status marker
            // (completed/failed) and the `octos clean` sweep.
            let worker_worktree = worker_worktree_guard
                .take()
                .map(WorkerWorktreeGuard::disarm);

            // M8 Runtime Parity W2.B2: wrap `run_task` with single-shot
            // M8.9 recovery so the synchronous spawn path mirrors the
            // session-actor recovery contract.
            let result = run_task_with_m8_9_recovery(&worker, &subtask, &task_desc).await;
            let tool_result = match result {
                Ok(r) => {
                    // Review A F-004: run declared completion-phase validators
                    // against the child's artifacts before surfacing success.
                    // Matches `enforce_spawn_task_contract`'s gating for
                    // spawn-only tools and closes the "vacuous pass" hole in
                    // `contract_failure_summary` (which only reads the ledger).
                    let mut output = r.output;
                    let mut success = r.success;
                    if success {
                        if let Some(ref policy) = parent_workspace_policy {
                            if !policy.validation.validators.is_empty() {
                                match crate::workspace_contract::run_declared_validators(
                                    child_tools_handle.as_ref(),
                                    &child_working_dir,
                                    &policy.validation.validators,
                                    "spawn",
                                    crate::validators::ValidatorPhase::Completion,
                                    None,
                                    std::sync::Arc::from(create_sandbox(&self.sandbox)),
                                )
                                .await
                                {
                                    Ok(_) => {}
                                    Err(reason) => {
                                        success = false;
                                        output = format!(
                                            "Subagent failed: contract validator rejected child artifact: {reason}"
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // octos #997 (round-2 fix): in addition to the session-scope
                    // validator run above, ALSO run each project-scope policy
                    // at its OWN project root. The session run writes its
                    // outcome to `<session>/.octos/validator_outcomes.jsonl`,
                    // but `inspect_workspace_contract` reads from
                    // `<session>/<kind>/<slug>/.octos/validator_outcomes.jsonl`
                    // — so without this run a real valid deck whose project
                    // policy declares a hard-required validator (octos #997:
                    // `slides.mofa_slides.pptx_magic_bytes`) would surface as
                    // `ready = false`. Scope the iteration to the workflow's
                    // expected kind when available so a slides spawn does not
                    // run the sites validator chain.
                    if success {
                        let expected_kind =
                            workflow.as_ref().and_then(workflow_contract_project_kind);
                        let report = crate::workspace_contract::run_project_root_validators(
                            child_tools_handle.as_ref(),
                            &child_working_dir,
                            expected_kind,
                            &r.files_to_send,
                            std::sync::Arc::from(create_sandbox(&self.sandbox)),
                        )
                        .await;
                        if let Some(reason) = report.first_failure_reason() {
                            success = false;
                            output = format!(
                                "Subagent failed: project-scope validator rejected child artifact: {reason}"
                            );
                        }
                    }

                    ToolResult {
                        output,
                        success,
                        tokens_used: Some(r.token_usage),
                        ..Default::default()
                    }
                }
                Err(e) => ToolResult {
                    output: format!("Subagent failed: {e}"),
                    success: false,
                    ..Default::default()
                },
            };
            if let Some(worktree) = worker_worktree.as_ref() {
                worktree.mark_status(if tool_result.success {
                    "completed"
                } else {
                    "failed"
                });
            }
            Ok(tool_result)
        } else {
            // Background mode: fire-and-forget
            let (origin_channel, origin_chat_id) = self
                .origin
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let task_ledger_path = self
                .task_ledger_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned());
            let tracked_task_id = self.task_supervisor.as_ref().map(|supervisor| {
                supervisor.register_with_lineage(
                    &label,
                    &format!("spawn-{worker_id}"),
                    self.session_key.as_deref(),
                    task_ledger_path.as_deref(),
                )
            });
            // Cap refusal: `register_with_lineage` signals a per-session
            // child-fanout rejection with an empty-string sentinel. Spawning
            // anyway would run a detached worker that is invisible to
            // `task/list` and uncancellable (`cancel("")` no-ops), and the
            // terminal guard below would be armed under `""`. Refuse the
            // spawn synchronously so the LLM sees the cap instead of a fake
            // "started in background" handle. (`None` = no supervisor at all —
            // the legacy untracked path — which stays allowed.)
            if tracked_task_id.as_deref() == Some("") {
                tracing::error!(
                    label = %label,
                    "background spawn register refused (child fanout cap); not spawning"
                );
                return Ok(ToolResult {
                    output: format!(
                        "[TASK LIMIT] Cannot spawn background subagent '{label}': this \
                         session reached its background-task fanout cap. Wait for \
                         running tasks to finish (or cancel them) before spawning more. \
                         Do not retry immediately."
                    ),
                    success: false,
                    ..Default::default()
                });
            }
            let tracked_child_session_key = tracked_task_id.as_ref().and_then(|task_id| {
                self.task_supervisor
                    .as_ref()
                    .and_then(|supervisor| supervisor.get_task(task_id))
                    .and_then(|task| task.child_session_key)
            });
            if let (Some(supervisor), Some(task_id), Some(template)) = (
                self.task_supervisor.as_ref(),
                tracked_task_id.as_ref(),
                role_template,
            ) {
                supervisor.set_m13b_projection(
                    task_id,
                    Some("model".to_owned()),
                    Some(template.name.to_owned()),
                    Some(label.chars().take(160).collect()),
                    Some(0),
                    Some(template.runtime_policy_stamp(
                        "model",
                        &input.backend,
                        input.model.as_deref(),
                    )),
                );
            }
            // codex round-5 (orphan-sweep liveness): arm the RAII terminal
            // guard HERE, in the FOREGROUND, before the `tokio::spawn` below.
            // `register_with_lineage` above already persisted a non-terminal
            // `Spawned` row; arming the guard inside the spawned future left a
            // window where a fast next-turn orphan-sweep could see the row
            // non-terminal AND not-live and falsely reap a
            // scheduled-but-not-yet-polled detached child. Constructing it
            // synchronously within the spawning turn inserts the id into the
            // process-global live-set before the turn returns (turns are
            // serialized per session ⇒ this completes before any next-turn
            // sweep). Moved into the future below so its Drop (clear live-set +
            // drive an unfinished task to Failed) still fires at worker end.
            let foreground_terminal_guard: Option<TaskTerminalGuard> =
                match (self.task_supervisor.as_ref(), tracked_task_id.as_ref()) {
                    (Some(supervisor), Some(task_id)) => {
                        Some(TaskTerminalGuard::new(supervisor.clone(), task_id.clone()))
                    }
                    _ => None,
                };
            let llm = sub_llm;
            let memory = self.memory.clone();
            let working_dir = child_working_dir.clone();
            // When a deliverable contract was seeded, `working_dir` IS the
            // per-task output dir; surface its declared artifacts on completion.
            let deliverable_declared = deliverable_glob.is_some();
            // Kept for the completion-time auto-materialize salvage (below).
            let deliverable_glob_for_completion = deliverable_glob.clone();
            let inbound_tx = self.inbound_tx.clone();
            let wid = worker_id.clone();
            // Fix 3 (mini4 re-review): the child's transcript reporter needs
            // the PARENT's output router and the exact router session id
            // `read_task_output` derives (`agent:{tool_call_id}` with
            // `tool_call_id = "spawn-{worker_id}"` from the register above).
            let parent_output_router = self.parent_subagent_output_router.clone();
            let child_stream_callback = self.child_stream_callback.clone();
            let child_router_session_id = format!("agent:spawn-{worker_id}");
            // PR #1250 finding 1: the fanout-cap refusal above was the last
            // refusal point on the background path — the detached worker is
            // definitely dispatched below. Disarm the prune guard and move
            // the worktree handle into the background task.
            let detached_worker_worktree = worker_worktree_guard
                .take()
                .map(WorkerWorktreeGuard::disarm);
            let provider_policy = self.provider_policy.clone();
            // #1607 (codex-review follow-up): capture the session sandbox so the
            // detached background child registry can be built with it (see the
            // `with_builtins_and_sandbox` call inside the closure). Without this
            // the background child's validator registry stored `NoSandbox`, so a
            // workspace-declared command validator could escape to the host.
            let child_sandbox = self.sandbox.clone();
            let additional_instructions = input.additional_instructions;
            // Spawn iteration budget (caller's clamped value or the generous
            // spawn default), captured for the detached closure (`input` is not
            // `'static`/available inside it).
            let bg_max_iters = resolve_spawn_max_iterations(input.max_iterations);
            let default_worker_prompt = self.worker_prompt.clone();
            let bg_sender = self.background_result_sender.clone();
            let child_session_sender = self.child_session_sender.clone();
            let task_label = label.clone();
            let plugin_dirs = self.plugin_dirs.clone();
            let plugin_extra_env = self.plugin_extra_env.clone();
            let plugin_require_signed = self.plugin_require_signed;
            let child_tool_factories = self.child_tool_factories.clone();
            // Detached path: `self` is not available inside the `tokio::spawn`
            // closure below, so pre-build the child's native `spawn` delegate
            // HERE (foreground) and move it in. Same purpose as the sync path —
            // registering it under "spawn" binds the child's `spawn_agent` /
            // `delegate` so a nested spawn resolves instead of hitting the
            // delegate-less builtin. Kept concrete (not `Arc<dyn Tool>`) so the
            // closure can align its supervisor with the child registry before
            // registering (see the sync-path rationale).
            let child_spawn_template =
                self.child_spawn_clone(child_working_dir.clone(), &worker_id);
            let task_supervisor = self.task_supervisor.clone();
            let worker_config = self.worker_config.clone();
            let worker_embedder = self.embedder.clone();
            let workflow_metadata = workflow.clone();
            let parent_session_key = self.session_key.clone();
            let worker_hooks = self.hooks.clone();
            let hook_context_template = self.hook_context_template.clone();
            // Review A F-004: carry the parent workspace policy into the
            // background child task so the detached child inherits the same
            // compaction + validator contracts the sync spawn path honours.
            let child_workspace_policy = parent_workspace_policy.clone();
            // M8.10 follow-up (#649): snapshot the originating turn's
            // thread_id (= user message's client_message_id) at spawn
            // time so the late-arriving terminal payload can stamp it
            // onto the OutboundMessage metadata. Without this snapshot
            // the payload would inherit whatever the per-chat sticky
            // map happens to hold when the background task finalises,
            // which after fast-follow-up turns is the WRONG turn's
            // thread_id (cf. live mini3 trace, 2026-04-29).
            let originating_thread_id = ctx.reporter.thread_id().map(str::to_string);
            // Snapshot the originating LLM `tool_call_id` (carried on
            // `ToolContext.tool_id`) so the late-arriving terminal payload
            // surfaces it on `TurnSpawnCompleteEvent.tool_call_id`. The
            // client uses this to flip the in-flight chip from spinner to
            // checkmark without a race against a `task/updated` watcher.
            // Empty when the caller invoked the tool from a non-LLM
            // context (synthetic harness, recovery path); in that case
            // the field stays `None` on the wire.
            let originating_tool_call_id = if ctx.tool_id.is_empty() {
                None
            } else {
                Some(ctx.tool_id.clone())
            };
            // M8 Runtime Parity W2.B1: capture parent caches into the
            // detached background closure so the bg child Agent gets the
            // same FileStateCache + Router + SummaryGenerator as the sync
            // path. Without these the detached subagent silently runs
            // without M8.4/M8.7 wiring even when the session actor
            // configured everything.
            let parent_file_state_cache = self.parent_file_state_cache.clone();
            let parent_subagent_output_router = self.parent_subagent_output_router.clone();
            let parent_subagent_summary_generator = self.parent_subagent_summary_generator.clone();
            // Issue #1125: invoke the child prompt-context factory
            // SYNCHRONOUSLY here (before any `await`) so the fork captures
            // the parent transcript as it stood at spawn dispatch time.
            //
            // The previous wiring deferred this call into the detached
            // `tokio::spawn` below, AFTER awaiting
            // `dispatch_child_session_lifecycle`. If the parent recorded
            // another user turn during that await window, the factory
            // (which locks the live parent `ContextManager`) would fork a
            // POST-spawn snapshot — leaking messages that were not part
            // of the spawning turn into the background child's context.
            //
            // Fork-at-dispatch produces a `ContextManager` snapshot pinned
            // to the pre-spawn parent generation; the detached task just
            // installs the pre-baked manager on the child Agent without
            // touching the parent lock.
            let prebuilt_child_prompt_context_manager = self
                .child_prompt_context_manager_factory
                .as_ref()
                .and_then(|factory| {
                    factory(ChildPromptContextRequest {
                        parent_session_key: self.session_key.clone(),
                        child_session_key: tracked_child_session_key.clone(),
                        task_id: tracked_task_id.clone(),
                        worker_id: worker_id.to_string(),
                        task_label: label.clone(),
                    })
                });
            // Guard C (issue #607): snapshot the caller's spawn depth so
            // the detached child Agent dispatched below sees
            // `parent_depth + 1` and the [`MAX_SPAWN_DEPTH`] gate fires
            // after a bounded number of nests.
            let child_spawn_depth = ctx.spawn_depth.saturating_add(1);
            // Phase 2-D of the SessionScope migration: snapshot the
            // parent's scope before crossing into the detached
            // `tokio::spawn` task. Worktree-isolated children get a
            // precomputed scope with the same policy and a worker CWD.
            let child_session_scope = child_session_scope.clone();

            tokio::spawn(async move {
                // codex round-5: the terminal guard was armed in the
                // FOREGROUND (before this spawn) so the task id entered the
                // process-global live-set synchronously within the spawning
                // turn — closing the window where a fast next-turn orphan-sweep
                // could reap a scheduled-but-not-yet-polled detached child.
                // Move it in here so its Drop — which clears the live-set and
                // drives an unfinished task to Failed (so the TUI count
                // decrements instead of hanging on "N running") — still fires
                // when this worker future terminates. Idempotent on the normal
                // terminal arms below; Drop no-ops once the body marked the
                // task terminal itself.
                let _terminal_guard = foreground_terminal_guard;
                if let (Some(supervisor), Some(task_id)) =
                    (task_supervisor.as_ref(), tracked_task_id.as_ref())
                {
                    supervisor.mark_running(task_id);
                    if let Some(workflow) = workflow_metadata.as_ref() {
                        // Seed `runtime_detail.progress` with a small non-null
                        // value at workflow start. Without this, dashboards
                        // (and the e2e live-progress gate) see
                        // `runtime_detail.progress == null` for the entire
                        // initial phase on workflows that drive a
                        // `run_pipeline` graph rather than emitting their
                        // own `HarnessEvent::progress`. The deep_search
                        // built-in still overwrites this with finer values
                        // (~0.1, 0.4, 0.8, 1.0) as the pipeline cycles.
                        let mut start = workflow.clone();
                        start.progress = Some(workflow_phase_progress(&start.current_phase));
                        supervisor.mark_runtime_state(
                            task_id,
                            crate::task_supervisor::TaskRuntimeState::ExecutingTool,
                            encode_workflow_detail(&start),
                        );
                    }
                }

                if let (Some(task_id), Some(parent_session_key), Some(child_session_key)) = (
                    tracked_task_id.as_ref(),
                    parent_session_key.as_ref(),
                    tracked_child_session_key.as_ref(),
                ) {
                    let joined = dispatch_child_session_lifecycle(
                        child_session_sender.as_ref(),
                        ChildSessionLifecyclePayload {
                            kind: ChildSessionLifecycleKind::Spawned,
                            task_id: task_id.clone(),
                            task_label: task_label.clone(),
                            instruction: task_desc.clone(),
                            parent_session_key: parent_session_key.clone(),
                            child_session_key: child_session_key.clone(),
                            workflow_kind: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.workflow_kind.clone()),
                            current_phase: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.current_phase.clone()),
                            output_files: Vec::new(),
                            failure_action: None,
                            error: None,
                        },
                    )
                    .await;
                    record_child_session_lifecycle(
                        ChildSessionLifecycleKind::Spawned,
                        if joined { "dispatched" } else { "not_joined" },
                    );
                }

                let harness_event_sink = match (
                    task_supervisor.as_ref(),
                    tracked_task_id.as_ref(),
                    parent_session_key.as_ref(),
                ) {
                    (Some(supervisor), Some(task_id), Some(session_key)) => {
                        match HarnessEventSink::new(
                            supervisor.clone(),
                            task_id.clone(),
                            session_key.clone(),
                        ) {
                            Ok(sink) => Some(sink),
                            Err(error) => {
                                warn!(
                                    task_id = %task_id,
                                    session_key = %session_key,
                                    error = %error,
                                    "failed to create harness event sink; continuing without structured child progress"
                                );
                                None
                            }
                        }
                    }
                    _ => None,
                };
                let harness_event_sink_path = harness_event_sink.as_ref().map(|sink| sink.uri());

                // #1607 (codex-review follow-up): build the detached child
                // registry with the SESSION sandbox rather than the hardcoded
                // `NoSandbox` `with_builtins` stores. Its `child_tools_handle`
                // feeds `run_declared_validators` / `run_project_root_validators`
                // below, and `build_validator_runner` confines command
                // validators to `tools.sandbox()`. On hosts without a real
                // backend `create_sandbox` yields `NoSandbox` (unchanged).
                let mut tools = ToolRegistry::with_builtins_and_sandbox(
                    &working_dir,
                    create_sandbox(&child_sandbox),
                );
                // Load plugin tools so subagents can use fm_tts, etc.
                // Section B (codex review P1.1): inherit the parent's
                // require_signed gate.
                if !plugin_dirs.is_empty() {
                    let _ = crate::plugins::PluginLoader::load_into_with_options(
                        &mut tools,
                        &plugin_dirs,
                        &plugin_extra_env,
                        crate::plugins::PluginLoadOptions {
                            work_dir: Some(&working_dir),
                            synthesis_config: None,
                            require_signed: plugin_require_signed,
                            verified_cache_dir: None,
                        },
                    );
                    // SPEC-VENDOR-NODE-V1 HTTP tool discovery — this is the
                    // ASYNC spawn-background path (`tokio::spawn(async move
                    // { ... })`); the enclosing block returns `()` so there is
                    // no Err-propagation channel. The contract therefore
                    // diverges from the sync subagent and boot paths: a
                    // failed catalog fetch in a background subagent is logged
                    // and the subagent runs with whatever static tools loaded.
                    // Documented divergence from @ymote's Finding 2 contract;
                    // the sync subagent path (line ~2470) still hard-fails.
                    if let Err(e) =
                        crate::plugins::register_http_skills_on_startup(&mut tools, &plugin_dirs)
                            .await
                    {
                        warn!(
                            error = %e,
                            "subagent (background) HTTP tool discovery failed; continuing with static tools only"
                        );
                    }
                }
                for factory in &child_tool_factories {
                    tools.register_arc(factory());
                }
                // Bind the detached child's OWN native `spawn` delegate (see
                // the foreground `child_spawn_template` build). Mirrors the
                // sync path: bind the template to THIS child registry's own
                // (private, per-child) supervisor — the one that backs the
                // child's `ctx.task_supervisor` — so the nested spawn_agent's
                // before/after lookup resolves against the same map the
                // delegate registers into (see the sync-path rationale for the
                // isolation trade-off).
                let mut child_spawn_template = child_spawn_template;
                child_spawn_template.task_supervisor = Some(tools.supervisor());
                tools.register(child_spawn_template);
                // In subagent context, spawn_only tools should be regular tools —
                // the subagent IS the background, so no need to auto-background again.
                tools.clear_spawn_only();
                // RFC-1 fixup (codex P1): mirror the sync spawn path — clear
                // internal-hidden markers so subagent registries can call
                // dispatcher targets directly without going through
                // `mofa_make`.
                tools.clear_internal_hidden();
                // Preflight against the EFFECTIVE (post-deny) allow-list —
                // mirror the sync path so a manifest-forbidden tool that is
                // absent from this registry does not fail the spawn (codex P2).
                let availability_check = ensure_subagent_tools_available(
                    &tools,
                    &effective_allowed_tools,
                    allow_list_is_caller_explicit,
                )
                .map_err(|error| eyre::eyre!(error));
                let policy = build_subagent_tool_policy(
                    allowed_tools,
                    manifest_disallowed_tools,
                    workflow_metadata.as_ref(),
                );
                tools.apply_policy(&policy);
                if let Some(pp) = provider_policy {
                    tools.set_provider_policy(pp);
                }
                // Review A F-004: clone the child LLM provider before the
                // Agent takes ownership so it can also back an LLM-iterative
                // compaction summarizer if the parent policy requests one.
                let child_llm_for_compaction = llm.clone();
                // Fix 3 (mini4 re-review): stream the child's transcript into
                // the parent's router so `read_task_output` is a LIVE window
                // into the running child. Default was `SilentReporter` — the
                // child's assistant text and tool activity were dropped, and
                // the agent view could only ever show status.
                let child_transcript_reporter: Option<Arc<dyn crate::progress::ProgressReporter>> =
                    match (parent_output_router.as_ref(), tracked_task_id.as_ref()) {
                        (Some(router), Some(task_id)) => {
                            Some(Arc::new(SpawnChildTranscriptReporter {
                                router: router.clone(),
                                router_session_id: child_router_session_id.clone(),
                                task_id: task_id.clone(),
                                stream_offset: AtomicU64::new(0),
                                on_stream_chunk: child_stream_callback.clone(),
                            }))
                        }
                        _ => None,
                    };
                let mut worker = Agent::new(wid.clone(), llm, tools, memory)
                    // Guard C (issue #607): inherit the parent's spawn
                    // nesting depth + 1 so the detached child sees the
                    // higher value when its own spawn calls run.
                    .with_spawn_depth(child_spawn_depth);
                if let Some(reporter) = child_transcript_reporter {
                    worker = worker.with_reporter(reporter);
                }
                // Phase 2-D: inherit the parent's SessionScope so the
                // detached child sees the same filesystem contract as
                // the sync spawn path (see `child_session_scope`
                // snapshot at dispatch time).
                if let Some(scope) = child_session_scope.as_ref() {
                    worker = worker.with_session_scope(scope.clone());
                }
                // Keep an Arc to the child's tool registry for the
                // post-`run_task` validator invocation below.
                let child_tools_handle = worker.tool_registry().clone();
                let mut effective_config = worker_config.clone().unwrap_or_default();
                effective_config.suppress_auto_send_files = true;
                effective_config.max_iterations = bg_max_iters;
                worker = worker.with_config(effective_config);
                // M8 Runtime Parity W2.B1: apply parent caches to the
                // detached background child before it consumes any
                // user-facing instruction. See `with_parent_file_state_cache`
                // for the contract.
                if let Some(ref cache) = parent_file_state_cache {
                    worker = worker.with_file_state_cache(cache.clone());
                }
                if let Some(ref router) = parent_subagent_output_router {
                    worker = worker.with_subagent_output_router(router.clone());
                }
                if let Some(ref summary_gen) = parent_subagent_summary_generator {
                    worker = worker.with_subagent_summary_generator(summary_gen.clone());
                }
                // Embed-on-save + recall parity (codex P1): the DEFAULT
                // background mode builds its own worker inside this
                // detached closure — mirror the sync-path propagation or
                // background subagents keep storing vectorless episodes.
                if let Some(ref embedder) = worker_embedder {
                    worker = worker.with_embedder(embedder.clone());
                }
                if let Some(ref sink_path) = harness_event_sink_path {
                    worker = worker.with_harness_event_sink(sink_path.clone());
                }
                if let Some(ref hooks) = worker_hooks {
                    worker = worker.with_hooks(hooks.clone());
                }
                if let Some(ctx) = hook_context_template.as_ref().map(|ctx| HookContext {
                    session_id: tracked_child_session_key
                        .clone()
                        .or_else(|| ctx.session_id.clone()),
                    profile_id: ctx.profile_id.clone(),
                }) {
                    worker = worker.with_hook_context(ctx);
                }
                // Issue #1125: install the pre-spawn-snapshot child
                // prompt-context manager that we forked synchronously at
                // dispatch time (see `prebuilt_child_prompt_context_manager`
                // above). The factory was invoked BEFORE this `tokio::spawn`
                // entered any await so the fork is pinned to the parent
                // generation at SpawnTool dispatch — post-spawn user
                // messages on the parent cannot leak into this child.
                if let Some(manager) = prebuilt_child_prompt_context_manager {
                    worker = worker.with_prompt_context_manager(manager);
                }

                // Review A F-004: propagate the parent's declarative
                // compaction policy onto the background child. The detached
                // child would otherwise silently run without preflight
                // compaction even when the parent's workspace_policy.toml
                // declares one, undermining the contract the parent honours.
                if let Some(ref policy) = child_workspace_policy {
                    if let Some(compaction_policy) = policy.compaction.clone() {
                        let runner = match compaction_policy.summarizer {
                            crate::workspace_policy::CompactionSummarizerKind::LlmIterative => {
                                crate::compaction::CompactionRunner::with_provider(
                                    compaction_policy,
                                    child_llm_for_compaction,
                                )
                            }
                            crate::workspace_policy::CompactionSummarizerKind::Extractive => {
                                crate::compaction::CompactionRunner::new(compaction_policy)
                            }
                        }
                        .with_workspace_policy(policy);
                        worker = worker
                            .with_compaction_runner(Arc::new(runner))
                            .with_compaction_workspace(policy.clone());
                    }
                }

                let base_prompt = default_worker_prompt
                    .unwrap_or_else(|| crate::DEFAULT_WORKER_PROMPT.to_string());
                let full_prompt = match additional_instructions {
                    Some(extra) if !extra.is_empty() => format!("{base_prompt}\n\n{extra}"),
                    _ => base_prompt,
                };
                worker = worker.with_system_prompt(full_prompt);

                let subtask = Task::new(
                    TaskKind::Code {
                        instruction: task_desc.clone(),
                        files: vec![],
                    },
                    TaskContext {
                        working_dir: working_dir.clone(),
                        ..Default::default()
                    },
                );

                // M8 Runtime Parity W2.B2: wrap `run_task` with single-shot
                // M8.9 recovery for the detached background path too.
                let mut result = match availability_check {
                    Ok(()) => run_task_with_m8_9_recovery(&worker, &subtask, &task_desc).await,
                    Err(error) => Err(error),
                };
                if let Ok(task_result) = result.as_mut() {
                    maybe_generate_inline_research_podcast(
                        worker.tool_registry(),
                        workflow_metadata.as_ref(),
                        &task_desc,
                        task_result,
                    )
                    .await;
                }

                // Review A F-004: actively run declared completion-phase
                // validators before the existing ledger-read checks. The
                // pre-fix path relied on `resolve_background_terminal_files`
                // + ledger inspection, which trivially passed when the child
                // never ran validators (the ledger was empty). Running the
                // validators here guarantees the required rail is exercised
                // before any downstream gate consults the ledger.
                let mut contract_failure: Option<String> = None;
                if let (Ok(task_result), Some(policy)) =
                    (result.as_ref(), child_workspace_policy.as_ref())
                {
                    if task_result.success && !policy.validation.validators.is_empty() {
                        if let Err(reason) = crate::workspace_contract::run_declared_validators(
                            child_tools_handle.as_ref(),
                            &working_dir,
                            &policy.validation.validators,
                            "spawn",
                            crate::validators::ValidatorPhase::Completion,
                            None,
                            std::sync::Arc::from(create_sandbox(&child_sandbox)),
                        )
                        .await
                        {
                            contract_failure = Some(reason);
                        }
                    }
                }

                // octos #997 (round-2 fix): also run each project-scope
                // policy AT its OWN project root. The session-scope run above
                // writes to `<session>/.octos/validator_outcomes.jsonl`, but
                // `inspect_workspace_contract` reads from
                // `<session>/<kind>/<slug>/.octos/validator_outcomes.jsonl`.
                // Without this run a real valid deck whose project policy
                // declares a hard-required validator (octos #997:
                // `slides.mofa_slides.pptx_magic_bytes`) would surface as
                // `ready = false` because the persisted outcome is missing
                // from the path `inspect_workspace_contract` reads.
                if contract_failure.is_none()
                    && matches!(&result, Ok(task_result) if task_result.success)
                {
                    let expected_kind = workflow_metadata
                        .as_ref()
                        .and_then(workflow_contract_project_kind);
                    let bg_files_to_send: &[PathBuf] = match &result {
                        Ok(task_result) => &task_result.files_to_send,
                        Err(_) => &[],
                    };
                    let report = crate::workspace_contract::run_project_root_validators(
                        child_tools_handle.as_ref(),
                        &working_dir,
                        expected_kind,
                        bg_files_to_send,
                        std::sync::Arc::from(create_sandbox(&child_sandbox)),
                    )
                    .await;
                    if let Some(reason) = report.first_failure_reason() {
                        contract_failure = Some(format!(
                            "project-scope validator rejected child artifact: {reason}"
                        ));
                    }
                }

                if contract_failure.is_none() {
                    contract_failure = match &result {
                        Ok(task_result) if task_result.success => {
                            resolve_background_terminal_files(
                                &working_dir,
                                &task_result.files_to_send,
                                &task_result.files_modified,
                                workflow_metadata.as_ref(),
                            )
                            .err()
                        }
                        _ => None,
                    };
                }
                let mut terminal_files = match (&result, contract_failure.as_ref()) {
                    (Ok(task_result), None) if task_result.success => {
                        resolve_background_terminal_files(
                            &working_dir,
                            &task_result.files_to_send,
                            &task_result.files_modified,
                            workflow_metadata.as_ref(),
                        )
                        .unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                // Deliverable contract: surface whatever the worker left in its
                // seeded output dir, however it was written (a `shell` heredoc
                // reports no `file_modified`, so the tool-record path above
                // misses it). `working_dir` IS that output dir when a
                // deliverable was declared. Only on success.
                if deliverable_declared && matches!(&result, Ok(task_result) if task_result.success)
                {
                    for path in resolve_deliverable_terminal_files(&working_dir) {
                        if !terminal_files.contains(&path) {
                            terminal_files.push(path);
                        }
                    }
                    // Auto-materialize salvage: the child declared a deliverable
                    // but wrote NO matching file, yet returned substantial final
                    // text — it delivered the work INLINE instead of to a file
                    // (mini4 live soak: a MiniMax reviewer returned a 14 KB
                    // review as its final answer, files_modified=0; another
                    // explore-looped to the iteration cap). Write that text to
                    // the declared deliverable path so the artifact exists and
                    // the parent's output pipeline surfaces it, instead of the
                    // work being lost.
                    if terminal_files.is_empty() {
                        if let Ok(task_result) = &result {
                            let body = task_result.output.trim();
                            if body.len() >= DELIVERABLE_AUTOMATERIALIZE_MIN_BYTES {
                                let name = derive_deliverable_filename(
                                    deliverable_glob_for_completion.as_deref().unwrap_or("*.md"),
                                    &task_label,
                                );
                                let path = working_dir.join(&name);
                                match std::fs::write(&path, task_result.output.as_bytes()) {
                                    Ok(()) => {
                                        info!(
                                            worker = %wid,
                                            file = %path.display(),
                                            bytes = body.len(),
                                            "auto-materialized deliverable from child final output"
                                        );
                                        terminal_files.push(path);
                                    }
                                    Err(error) => warn!(
                                        worker = %wid,
                                        %error,
                                        "failed to auto-materialize deliverable from final output"
                                    ),
                                }
                            }
                        }
                    }
                }
                let workflow_kind = workflow_metadata
                    .as_ref()
                    .map(|workflow| workflow.workflow_kind.clone());
                let workflow_phase = workflow_metadata
                    .as_ref()
                    .map(|workflow| workflow.current_phase.clone());
                let verify_phase = workflow_phase
                    .clone()
                    .or_else(|| Some("verify_outputs".to_string()));

                if matches!((&result, contract_failure.as_ref()), (Ok(task_result), None) if task_result.success)
                {
                    if let (Some(task_id), Some(parent_session_key), Some(child_session_key)) = (
                        tracked_task_id.as_ref(),
                        parent_session_key.as_ref(),
                        tracked_child_session_key.as_ref(),
                    ) {
                        let before_verify_payload = HookPayload::before_spawn_verify(
                            task_id.clone(),
                            task_label.clone(),
                            parent_session_key.clone(),
                            child_session_key.clone(),
                            workflow_kind.clone(),
                            verify_phase.clone(),
                            Some("candidate terminal outputs resolved"),
                            terminal_files
                                .iter()
                                .map(|path| path.to_string_lossy().to_string())
                                .collect(),
                            hook_context_template.as_ref(),
                        );
                        match run_before_spawn_verify_hook(
                            worker_hooks.as_ref(),
                            before_verify_payload,
                        )
                        .await
                        {
                            Ok(modified_files) => {
                                terminal_files = modified_files;
                            }
                            Err(reason) => {
                                contract_failure =
                                    Some(format!("spawn verify denied by hook: {reason}"));
                                terminal_files.clear();
                            }
                        }
                    }
                }

                let tracked_output_files = terminal_files
                    .iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect::<Vec<_>>();

                if matches!((&result, contract_failure.as_ref()), (Ok(task_result), None) if task_result.success)
                {
                    if let (Some(task_id), Some(parent_session_key), Some(child_session_key)) = (
                        tracked_task_id.as_ref(),
                        parent_session_key.as_ref(),
                        tracked_child_session_key.as_ref(),
                    ) {
                        emit_lifecycle_hook(
                            worker_hooks.as_ref(),
                            HookPayload::on_spawn_verify(
                                task_id.clone(),
                                task_label.clone(),
                                parent_session_key.clone(),
                                child_session_key.clone(),
                                workflow_kind.clone(),
                                verify_phase.clone(),
                                Some("terminal outputs resolved"),
                                tracked_output_files.clone(),
                                hook_context_template.as_ref(),
                            ),
                        )
                        .await;
                    }
                }

                if matches!(&result, Ok(task_result) if task_result.success) {
                    if let (Some(supervisor), Some(task_id), Some(workflow)) = (
                        task_supervisor.as_ref(),
                        tracked_task_id.as_ref(),
                        workflow_metadata.as_ref(),
                    ) {
                        let mut deliver = workflow.clone();
                        deliver.current_phase = "deliver_result".to_string();
                        deliver.progress = Some(workflow_phase_progress("deliver_result"));
                        supervisor.mark_runtime_state(
                            task_id,
                            crate::task_supervisor::TaskRuntimeState::DeliveringOutputs,
                            encode_workflow_detail(&deliver),
                        );
                    }
                }

                let terminal_kind = if contract_failure.is_some() {
                    ChildSessionLifecycleKind::TerminalFailed
                } else {
                    classify_child_session_lifecycle_kind(&result)
                };
                if let Some(worktree) = detached_worker_worktree.as_ref() {
                    worktree.mark_status(match terminal_kind {
                        ChildSessionLifecycleKind::Completed => "completed",
                        ChildSessionLifecycleKind::RetryableFailed
                        | ChildSessionLifecycleKind::TerminalFailed => "failed",
                        ChildSessionLifecycleKind::Spawned => "spawned",
                    });
                }

                if let (Some(supervisor), Some(task_id)) =
                    (task_supervisor.as_ref(), tracked_task_id.as_ref())
                {
                    match (&result, contract_failure.as_ref()) {
                        (Ok(task_result), None) if task_result.success => {
                            supervisor.mark_completed(task_id, tracked_output_files.clone());
                        }
                        (Ok(_), Some(error)) => {
                            supervisor.mark_failed(task_id, error.clone());
                        }
                        (Ok(task_result), None) => {
                            supervisor.mark_failed(task_id, task_result.output.clone());
                        }
                        (Err(error), _) => {
                            supervisor.mark_failed(task_id, error.to_string());
                        }
                    }
                }

                let terminal_result_text = match (&result, contract_failure.as_ref()) {
                    (Ok(_), Some(error)) => error.clone(),
                    (Ok(task_result), None) => task_result.output.clone(),
                    (Err(error), _) => error.to_string(),
                };

                if let (Some(task_id), Some(parent_session_key), Some(child_session_key)) = (
                    tracked_task_id.as_ref(),
                    parent_session_key.as_ref(),
                    tracked_child_session_key.as_ref(),
                ) {
                    let payload = match (&result, contract_failure.as_ref()) {
                        (Ok(task_result), None) if task_result.success => {
                            ChildSessionLifecyclePayload {
                                kind: terminal_kind,
                                task_id: task_id.clone(),
                                task_label: task_label.clone(),
                                instruction: task_desc.clone(),
                                parent_session_key: parent_session_key.clone(),
                                child_session_key: child_session_key.clone(),
                                workflow_kind: workflow_metadata
                                    .as_ref()
                                    .map(|workflow| workflow.workflow_kind.clone()),
                                current_phase: Some("deliver_result".to_string()),
                                output_files: tracked_output_files.clone(),
                                failure_action: child_session_failure_action(terminal_kind),
                                error: None,
                            }
                        }
                        (Ok(_), Some(error)) => ChildSessionLifecyclePayload {
                            kind: terminal_kind,
                            task_id: task_id.clone(),
                            task_label: task_label.clone(),
                            instruction: task_desc.clone(),
                            parent_session_key: parent_session_key.clone(),
                            child_session_key: child_session_key.clone(),
                            workflow_kind: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.workflow_kind.clone()),
                            current_phase: Some("deliver_result".to_string()),
                            output_files: tracked_output_files.clone(),
                            failure_action: child_session_failure_action(terminal_kind),
                            error: Some(error.clone()),
                        },
                        (Ok(task_result), None) => ChildSessionLifecyclePayload {
                            kind: terminal_kind,
                            task_id: task_id.clone(),
                            task_label: task_label.clone(),
                            instruction: task_desc.clone(),
                            parent_session_key: parent_session_key.clone(),
                            child_session_key: child_session_key.clone(),
                            workflow_kind: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.workflow_kind.clone()),
                            current_phase: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.current_phase.clone()),
                            output_files: tracked_output_files.clone(),
                            failure_action: child_session_failure_action(terminal_kind),
                            error: Some(task_result.output.clone()),
                        },
                        (Err(error), _) => ChildSessionLifecyclePayload {
                            kind: terminal_kind,
                            task_id: task_id.clone(),
                            task_label: task_label.clone(),
                            instruction: task_desc.clone(),
                            parent_session_key: parent_session_key.clone(),
                            child_session_key: child_session_key.clone(),
                            workflow_kind: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.workflow_kind.clone()),
                            current_phase: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.current_phase.clone()),
                            output_files: tracked_output_files.clone(),
                            failure_action: child_session_failure_action(terminal_kind),
                            error: Some(error.to_string()),
                        },
                    };
                    let joined =
                        dispatch_child_session_lifecycle(child_session_sender.as_ref(), payload)
                            .await;
                    record_child_session_lifecycle(
                        terminal_kind,
                        if joined { "dispatched" } else { "not_joined" },
                    );
                    if let Some(supervisor) = task_supervisor.as_ref() {
                        if let Some(task_id) = tracked_task_id.as_ref() {
                            let terminal_state = match terminal_kind {
                                ChildSessionLifecycleKind::Completed => {
                                    crate::task_supervisor::ChildSessionTerminalState::Completed
                                }
                                ChildSessionLifecycleKind::RetryableFailed => {
                                    crate::task_supervisor::ChildSessionTerminalState::RetryableFailure
                                }
                                ChildSessionLifecycleKind::TerminalFailed => {
                                    crate::task_supervisor::ChildSessionTerminalState::TerminalFailure
                                }
                                ChildSessionLifecycleKind::Spawned => unreachable!(
                                    "child session terminal handling should never see Spawned"
                                ),
                            };
                            supervisor.mark_child_session_outcome(
                                task_id,
                                terminal_state,
                                if joined {
                                    crate::task_supervisor::ChildSessionJoinState::Joined
                                } else {
                                    crate::task_supervisor::ChildSessionJoinState::Orphaned
                                },
                            );
                        }
                    }
                }

                if let (Some(task_id), Some(parent_session_key), Some(child_session_key)) = (
                    tracked_task_id.as_ref(),
                    parent_session_key.as_ref(),
                    tracked_child_session_key.as_ref(),
                ) {
                    match terminal_kind {
                        ChildSessionLifecycleKind::Completed => {
                            emit_lifecycle_hook(
                                worker_hooks.as_ref(),
                                HookPayload::on_spawn_complete(
                                    task_id.clone(),
                                    task_label.clone(),
                                    parent_session_key.clone(),
                                    child_session_key.clone(),
                                    workflow_kind.clone(),
                                    Some("deliver_result".to_string()),
                                    Some(terminal_result_text.clone()),
                                    tracked_output_files.clone(),
                                    hook_context_template.as_ref(),
                                ),
                            )
                            .await;
                        }
                        ChildSessionLifecycleKind::RetryableFailed
                        | ChildSessionLifecycleKind::TerminalFailed => {
                            let failure_action = child_session_failure_action(terminal_kind)
                                .map(child_session_failure_action_label)
                                .unwrap_or("escalate");
                            emit_lifecycle_hook(
                                worker_hooks.as_ref(),
                                HookPayload::on_spawn_failure(
                                    task_id.clone(),
                                    task_label.clone(),
                                    parent_session_key.clone(),
                                    child_session_key.clone(),
                                    workflow_kind.clone(),
                                    workflow_phase.clone(),
                                    terminal_result_text.clone(),
                                    tracked_output_files.clone(),
                                    failure_action,
                                    hook_context_template.as_ref(),
                                ),
                            )
                            .await;
                        }
                        ChildSessionLifecycleKind::Spawned => {}
                    }
                }

                // C1 step 3: derive the terminal supervisor status from the
                // SAME (&result, contract_failure) match the mark_* arms above
                // used, so the BackgroundResultPayload carries an explicit
                // outcome the session actor can read instead of inferring
                // success from the rendered content string.
                let terminal_status = match (&result, contract_failure.as_ref()) {
                    (Ok(task_result), None) if task_result.success => {
                        crate::task_supervisor::TaskStatus::Completed
                    }
                    _ => crate::task_supervisor::TaskStatus::Failed,
                };
                let content = match (&result, contract_failure.as_ref()) {
                    (Ok(_), Some(error)) => format!("Status: FAILED\nError: {error}"),
                    (Ok(r), None) => format!(
                        "Status: {}\n\n{}",
                        if r.success { "SUCCESS" } else { "FAILED" },
                        r.output
                    ),
                    (Err(e), _) => format!("Status: FAILED\nError: {e}"),
                };
                // Durably record the child's FULL result on the task record
                // before any delivery preview/truncation downstream. This is
                // what `read_task_output` falls back to for spawn children
                // (whose transcript never flows through the output router) —
                // without it, the parent's only copies were a truncated
                // announce and an empty router read, and models concluded
                // the child's result "was lost" (mini4 re-review forensic).
                if let (Some(supervisor), Some(task_id)) =
                    (task_supervisor.as_ref(), tracked_task_id.as_ref())
                {
                    supervisor.record_final_output(task_id, &content);
                }
                let (result_kind, result_media) = match (&result, contract_failure.as_ref()) {
                    (Ok(_), Some(_)) => {
                        record_terminal_result_reason(
                            BackgroundResultKind::Report,
                            "workspace_contract_failure",
                        );
                        (BackgroundResultKind::Report, Vec::new())
                    }
                    (Ok(r), None) if r.success => {
                        if !terminal_files.is_empty() {
                            record_terminal_result_reason(
                                BackgroundResultKind::Notification,
                                "workflow_terminal_artifact",
                            );
                            (
                                BackgroundResultKind::Notification,
                                terminal_files
                                    .into_iter()
                                    .map(|path| path.to_string_lossy().to_string())
                                    .collect::<Vec<_>>(),
                            )
                        } else if should_deliver_output_files(&r.files_to_send) {
                            record_terminal_result_reason(
                                BackgroundResultKind::Notification,
                                "explicit_output_files",
                            );
                            (
                                BackgroundResultKind::Notification,
                                r.files_to_send
                                    .iter()
                                    .map(|path| path.to_string_lossy().to_string())
                                    .collect::<Vec<_>>(),
                            )
                        } else {
                            record_terminal_result_reason(
                                BackgroundResultKind::Report,
                                "report_summary",
                            );
                            (BackgroundResultKind::Report, Vec::new())
                        }
                    }
                    _ => {
                        record_terminal_result_reason(
                            BackgroundResultKind::Report,
                            "task_failure_report",
                        );
                        (BackgroundResultKind::Report, Vec::new())
                    }
                };

                // Direct injection path: inject as system message, no extra LLM call.
                // If the actor has exited (idle timeout), the send fails and we
                // fall through to the legacy InboundMessage relay path.
                if deliver_background_result(
                    bg_sender,
                    BackgroundResultPayload {
                        task_label,
                        content: content.clone(),
                        kind: result_kind,
                        media: result_media.clone(),
                        envelope_media: vec![],
                        originating_thread_id: originating_thread_id.clone(),
                        task_id: tracked_task_id.clone(),
                        // Issue #960: same value as `originating_thread_id`
                        // — the reporter's `thread_id()` is the user's
                        // `client_message_id` on the gateway/cmid-bound
                        // path and the `TurnId` UUID on the WS path; the
                        // SPA reducer's thread-map keys on whichever shape
                        // its parent prompt row carries.
                        originating_client_message_id: originating_thread_id.clone(),
                        tool_call_id: originating_tool_call_id.clone(),
                        terminal_status: Some(terminal_status),
                    },
                )
                .await
                {
                    return;
                }
                record_retry("background_result_relay_fallback");
                warn!("background result sender failed (actor dead?), falling back to relay");

                // Legacy path: relay via InboundMessage (triggers extra LLM call)
                let content = match &result {
                    Ok(r) => format!(
                        "[Subagent {} completed]\nTask: {}\nStatus: {}\n\nResult:\n{}\n\nPlease summarize this result naturally for the user.",
                        wid,
                        task_desc,
                        if r.success { "SUCCESS" } else { "FAILED" },
                        r.output
                    ),
                    Err(e) => format!(
                        "[Subagent {} failed]\nTask: {}\nError: {e}\n\nPlease inform the user about this failure.",
                        wid, task_desc
                    ),
                };

                let announce = InboundMessage {
                    channel: "system".into(),
                    sender_id: "subagent".into(),
                    chat_id: format!("{origin_channel}:{origin_chat_id}"),
                    content,
                    timestamp: chrono::Utc::now(),
                    media: vec![],
                    metadata: serde_json::json!({
                        "deliver_to_channel": origin_channel,
                        "deliver_to_chat_id": origin_chat_id,
                    }),
                    message_id: None,
                    origin: octos_core::MessageOrigin::Synthetic,
                };

                if let Err(e) = inbound_tx.send(announce).await {
                    record_result_delivery("relay_inbound_message", "enqueue_failed", result_kind);
                    warn!(error = %e, "failed to announce subagent result");
                } else {
                    record_result_delivery("relay_inbound_message", "enqueued", result_kind);
                }
            });

            Ok(ToolResult {
                output: format!("Spawned background task: {label}"),
                success: true,
                ..Default::default()
            })
        }
    }
}

#[cfg(test)]
#[path = "spawn_tests.rs"]
mod tests;
