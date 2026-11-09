//! Shell tool for executing commands.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use tokio::time::timeout;

use super::{
    ConcurrencyClass, TOOL_APPROVAL_CTX, TOOL_CTX, Tool, ToolApprovalDecision, ToolApprovalRequest,
    ToolContext, ToolResult,
};
use crate::policy::{ApprovalPolicy, CommandPolicy, Decision, SafePolicy};
use crate::sandbox::{NoSandbox, Sandbox};
use crate::subprocess_env::{EnvAllowlist, sanitize_command_env};
use crate::task_supervisor::TaskSupervisor;
use crate::tools::policy::BashFileWrites;

/// Monotonic sequence used to synthesise a `tool_call_id` for background shell
/// tasks when the caller did not thread one through `ToolContext` (e.g. the
/// legacy `execute()` entry point used by tests). Production callers always
/// carry a real tool id, so this only fires off the hot path.
static SHELL_BG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Tool for executing shell commands.
pub struct ShellTool {
    /// Timeout for command execution.
    timeout: Duration,
    /// Working directory for commands.
    cwd: std::path::PathBuf,
    /// Policy for command approval.
    policy: Arc<dyn CommandPolicy>,
    /// #28b — loaded ONCE at construction (never re-read per call).
    bash_file_writes: crate::tools::policy::BashFileWrites,
    /// Runtime approval behavior for commands that request approval.
    approval_policy: ApprovalPolicy,
    /// Sandbox for command isolation.
    sandbox: Arc<dyn Sandbox>,
    /// Optional shared task supervisor. When set (or supplied via
    /// [`ToolContext::task_supervisor`]), background shell commands — a
    /// trailing `&` or an explicit `background: true` arg — are registered as
    /// tracked tasks so they surface in `/ps` and the sub-agent dock. Mirrors
    /// [`crate::tools::spawn::SpawnTool`]'s supervisor wiring.
    task_supervisor: Option<Arc<TaskSupervisor>>,
    /// Session key used to tag registered background tasks (links the task to
    /// its owning session in `/ps`). Falls back to
    /// [`ToolContext::parent_session_key`] when unset.
    session_key: Option<String>,
    /// Task-ledger path recorded as lineage on registered background tasks so
    /// they can be restored across a restart, mirroring the spawn tool.
    task_ledger_path: Option<PathBuf>,
    /// When `false`, a background/detached command — an explicit
    /// `background: true` arg OR a trailing `&` — is refused outright instead
    /// of being spawned detached. A detached child outlives the agent turn and
    /// dodges any per-attempt deadline, so a closed, non-interactive worker
    /// (e.g. the fleet task-worker) sets this `false` to stay replay-safe.
    /// Defaults to `true` — unchanged behaviour for every existing caller.
    background_allowed: bool,
    /// Optional hard CEILING on a single command's effective timeout. When set,
    /// the per-command timeout is `min(requested-or-default, max_timeout)` — an
    /// upper bound that overrides a LARGER LLM-provided `timeout_secs`, so a
    /// closed worker can guarantee no foreground command outlives its attempt
    /// deadline. `None` = no extra cap (only the outer `[1, 600]s` clamp
    /// applies) — unchanged behaviour for every existing caller.
    max_timeout: Option<Duration>,
}

impl ShellTool {
    /// Create a new shell tool with safe defaults.
    pub fn new(cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            timeout: Duration::from_secs(120),
            cwd: cwd.into(),
            policy: Arc::new(SafePolicy::default()),
            bash_file_writes: BashFileWrites::default(),
            approval_policy: ApprovalPolicy::Ask,
            sandbox: Arc::new(NoSandbox),
            task_supervisor: None,
            session_key: None,
            task_ledger_path: None,
            background_allowed: true,
            max_timeout: None,
        }
    }

    /// Cap the effective per-command timeout at `cap_secs` (a hard CEILING).
    ///
    /// The effective timeout becomes `min(requested-or-default, cap_secs)`, so
    /// this overrides a LARGER LLM-provided `timeout_secs` (which the agent
    /// loop can otherwise raise up to the outer 600s clamp). A closed worker
    /// sets this to its attempt deadline so — together with
    /// `with_background_allowed(false)` — no single foreground command can
    /// outlive the deadline. `cap_secs` is floored to 1. Default: no extra cap.
    pub fn with_max_timeout_secs(mut self, cap_secs: u64) -> Self {
        self.max_timeout = Some(Duration::from_secs(cap_secs.max(1)));
        self
    }

    /// Allow or forbid background/detached execution (default: allowed).
    ///
    /// With `allowed = false`, any background request — an explicit
    /// `background: true` arg or a trailing `&` — is refused with a failed
    /// [`ToolResult`] rather than spawned detached. A closed worker that must
    /// not outlive its per-attempt deadline (the fleet task-worker) sets this
    /// so a `sh -c "cmd &"` cannot orphan work past the attempt.
    pub fn with_background_allowed(mut self, allowed: bool) -> Self {
        self.background_allowed = allowed;
        self
    }

    /// Set the timeout for commands.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set a custom command policy.
    /// #28b — set the bash file-writes knob (defaults to `Allow`).
    pub fn with_bash_file_writes(mut self, mode: crate::tools::policy::BashFileWrites) -> Self {
        self.bash_file_writes = mode;
        self
    }

    pub fn with_policy(mut self, policy: Arc<dyn CommandPolicy>) -> Self {
        self.policy = policy;
        self
    }

    /// Set the runtime approval behavior.
    pub fn with_approval_policy(mut self, approval_policy: ApprovalPolicy) -> Self {
        self.approval_policy = approval_policy;
        self
    }

    /// Set a sandbox for command isolation.
    pub fn with_sandbox(mut self, sandbox: Box<dyn Sandbox>) -> Self {
        self.sandbox = Arc::from(sandbox);
        self
    }

    /// Set a shared sandbox for command isolation.
    pub fn with_shared_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Register background shell commands (a trailing `&` or an explicit
    /// `background: true` arg) as tracked tasks in the shared
    /// [`TaskSupervisor`] so they surface in `/ps` and the sub-agent dock.
    ///
    /// Mirrors [`crate::tools::spawn::SpawnTool::with_task_supervisor`]: the
    /// same three-tuple of supervisor + session key + task-ledger path. In
    /// production the foreground executor already threads the SSOT supervisor
    /// and session key onto every tool call via
    /// [`ToolContext::task_supervisor`] / [`ToolContext::parent_session_key`],
    /// so this builder is primarily for explicit construction and tests; at
    /// execute time the explicit handle wins and the context is the fallback.
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

    /// Run a background command detached and return immediately.
    ///
    /// The command still runs through the same sandbox/env path as a
    /// foreground command (policy/approval were already enforced by the
    /// caller). When a supervisor is available — the explicit builder handle
    /// or [`ToolContext::task_supervisor`] — the command is registered as a
    /// tracked task (status `running`) and a lightweight watcher flips it to
    /// terminal (`completed`/`failed`) when the child exits, so `/ps` shows
    /// the running→done transition. Without a supervisor the command still
    /// runs detached and is reaped, but is untracked.
    async fn execute_background(
        &self,
        ctx: &ToolContext,
        raw_command: &str,
        effective_cwd: &Path,
    ) -> ToolResult {
        // Strip the trailing `&` so OUR child is the actual work (see
        // `strip_trailing_ampersand`).
        let command = strip_trailing_ampersand(raw_command);

        let usage_guard = match begin_build_cache_use(ctx) {
            Ok(guard) => guard,
            Err(message) => {
                return ToolResult {
                    output: message.to_owned(),
                    success: false,
                    ..Default::default()
                };
            }
        };
        let slot = resolve_build_cache_slot(ctx);
        let mut cmd = self.sandbox.wrap_command_with_build_cache_slot(
            &command,
            effective_cwd,
            slot.as_deref(),
        );
        // We return immediately and never read the pipes, so piping stdout/
        // stderr risks a full-buffer deadlock in a chatty child. Discard the
        // std streams (in-command redirects like `> log 2>&1` still win).
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_frontend_tool_env(&mut cmd, effective_cwd);
        apply_quarto_tool_env(&mut cmd, &command, effective_cwd);
        apply_git_tool_env(&mut cmd, &command);
        sanitize_command_env(&mut cmd, &EnvAllowlist::empty());
        apply_build_cache_env(&mut cmd, slot.as_deref());
        apply_harness_event_sink_env(&mut cmd, ctx);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    output: format!("Failed to start background command: {e}"),
                    success: false,
                    ..Default::default()
                };
            }
        };
        let child_pid = child.id();
        let label = background_label(&command);

        // Resolve the supervisor + session key: an explicit builder handle
        // wins, else fall back to the per-turn ToolContext (the foreground
        // executor threads the SSOT supervisor here).
        let supervisor = self
            .task_supervisor
            .clone()
            .or_else(|| ctx.task_supervisor.clone());
        let session_key = self
            .session_key
            .clone()
            .or_else(|| ctx.parent_session_key.clone());

        // Register a tracked task (mirrors spawn's `register_with_lineage`).
        let task_id = supervisor.as_ref().map(|sup| {
            let tool_call_id = if ctx.tool_id.is_empty() {
                let seq = SHELL_BG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                format!("shell-bg-{seq}")
            } else {
                ctx.tool_id.clone()
            };
            let ledger = self
                .task_ledger_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned());
            sup.register_with_lineage(
                &label,
                &tool_call_id,
                session_key.as_deref(),
                ledger.as_deref(),
            )
        });

        match (supervisor, task_id) {
            // Tracked: spawn a watcher that flips the task terminal on exit.
            (Some(sup), Some(id)) if !id.is_empty() => {
                // Flip Spawned → Running (the child is already executing), so
                // `/ps` shows an active task, mirroring the spawn tool.
                sup.mark_running(&id);
                let watch_id = id.clone();
                let watch_label = label.clone();
                tokio::spawn(async move {
                    let _usage_guard = usage_guard;
                    let mut child = child;
                    match child.wait().await {
                        Ok(status) if status.success() => sup.mark_completed(&watch_id, vec![]),
                        Ok(status) => sup.mark_failed(
                            &watch_id,
                            format!(
                                "{watch_label} exited with status {}",
                                status.code().unwrap_or(-1)
                            ),
                        ),
                        Err(e) => sup.mark_failed(
                            &watch_id,
                            format!("failed to wait on background command: {e}"),
                        ),
                    }
                });
                ToolResult {
                    output: format!(
                        "Started background task {id} (pid {}): {label}",
                        child_pid
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "?".to_string())
                    ),
                    success: true,
                    ..Default::default()
                }
            }
            // Untracked (no supervisor, or fan-out cap refused the register):
            // still run detached, but reap the child so it doesn't zombie.
            _ => {
                tokio::spawn(async move {
                    let _usage_guard = usage_guard;
                    let mut child = child;
                    let _ = child.wait().await;
                });
                ToolResult {
                    output: format!(
                        "Started background command (untracked, pid {}): {label}",
                        child_pid
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "?".to_string())
                    ),
                    success: true,
                    ..Default::default()
                }
            }
        }
    }
}

fn frontend_tool_cache_dir(cwd: &Path) -> PathBuf {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let cache_key = cwd
        .to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    let preferred = std::env::temp_dir()
        .join("octos-frontend-tool-cache")
        .join(user)
        .join(cache_key);
    let _ = std::fs::create_dir_all(&preferred);
    preferred
}

fn apply_frontend_tool_env(cmd: &mut tokio::process::Command, cwd: &Path) {
    let cache_dir = frontend_tool_cache_dir(cwd);
    cmd.env("ASTRO_TELEMETRY_DISABLED", "1")
        .env("NPM_CONFIG_CACHE", &cache_dir)
        .env("npm_config_cache", &cache_dir);
}

/// True when `command` runs the `quarto` CLI as a command (not merely
/// mentions it in a path). Splitting on shell separators means
/// `cd sites/foo && quarto render` is detected via its `quarto render`
/// segment.
///
/// A segment's leading `NAME=value` env-assignment prefixes are skipped
/// before testing the command word, so `QUARTO_PROJECT_DIR=. quarto
/// render` is still recognised — without this, the inline-env form left
/// HOME unredirected and quarto's sass cache escaped the sandbox
/// (`unable to open database file: …sass.kv`).
fn command_invokes_quarto(command: &str) -> bool {
    command
        .split(['\n', ';', '&', '|'])
        .map(str::trim)
        .any(segment_runs_quarto)
}

/// True when the first command word of a single shell segment is
/// `quarto`, after skipping any leading `NAME=value` env assignments.
fn segment_runs_quarto(segment: &str) -> bool {
    segment
        .split_whitespace()
        .find(|token| !is_env_assignment(token))
        == Some("quarto")
}

/// True for a `NAME=value` shell env-assignment token (the form that may
/// legally precede a command word, e.g. `FOO=bar cmd`).
fn is_env_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().enumerate().all(|(i, c)| {
                    c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                })
        }
        None => false,
    }
}

/// Directory used as `$HOME` for quarto invocations. Quarto writes its
/// sass cache to `$HOME/Library/Caches/quarto` and ignores
/// `QUARTO_CACHE`/`XDG_CACHE_HOME` on macOS, so under the sandbox
/// (writes confined to cwd) the default home cache is denied and
/// `quarto render` fails with `unable to open database file: …sass.kv`.
/// Pointing HOME at a dir inside the (writable) workspace keeps the
/// cache contained without weakening sandbox isolation.
fn quarto_home_dir(cwd: &Path) -> PathBuf {
    cwd.join(".octos-quarto-home")
}

fn apply_quarto_tool_env(cmd: &mut tokio::process::Command, command: &str, cwd: &Path) {
    if !command_invokes_quarto(command) {
        return;
    }
    let home = quarto_home_dir(cwd);
    let _ = std::fs::create_dir_all(&home);
    cmd.env("HOME", &home);
}

#[cfg(windows)]
const NULL_DEVICE_PATH: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE_PATH: &str = "/dev/null";

fn contains_git_invocation(command: &str) -> bool {
    shell_command_segments(command)
        .iter()
        .any(|segment| segment_invokes_git(segment))
}

// ---------------------------------------------------------------------------
// #20b — main-tree sovereignty (树主权) checkout guard.
//
// The git TOOL is read-only; a branch switch on the MAIN TREE can only arrive
// through this shell tool. When the main tree (the session's workspace root)
// is owned by a goal — the first goal to land a non-default branch on it,
// recorded durably in that goal's ledger by the host via the provider below —
// a `git checkout` / `git switch` that would move the main tree's HEAD to a
// DIFFERENT branch, issued by any session that does not belong to the owner
// goal, is REFUSED with a "fence yourself" hint instead of silently switching.
//
// Scope discipline:
//   - Only fires when the command's effective cwd IS the main tree root —
//     checkouts inside a fenced peer clone (`peers/<slug>/wt`) pass through.
//   - Read-only git invocations (status/diff/log/…), plain `git init`, and
//     pathspec restores (`git checkout -- file`, `git checkout <file-that-
//     exists>`) never match the HEAD-moving detector.
//   - Fail-open: no provider wired (solo CLI, tests, hosts that never opted
//     in), unknown current branch (non-git dir, detached HEAD read failure),
//     or no recorded owner → the guard stays out of the way.
// ---------------------------------------------------------------------------

/// Snapshot the host hands the shell tool for the #20b sovereignty decision.
#[derive(Debug, Clone)]
pub struct MainTreeSovereigntyContext {
    /// Canonical main-tree (workspace) root the guard protects.
    pub main_tree_root: PathBuf,
    /// The main tree's CURRENT branch; `None` = unknown (non-git / detached /
    /// read failure) → fail open.
    pub main_tree_branch: Option<String>,
    /// Goal that owns the main tree (first goal to branch it off trunk);
    /// `None` = no owner → never block.
    pub owner_goal_id: Option<String>,
    /// Goal the CALLING session belongs to (`ToolContext::goal_id`).
    pub caller_goal_id: Option<String>,
}

/// #20b — the boxed sovereignty-provider closure type, extracted so neither
/// the `static` slot nor the setter signature trips `clippy::type_complexity`.
/// Called once per shell execution with the calling session's goal id;
/// returns `None` to disable the guard.
pub type MainTreeSovereigntyProvider =
    Arc<dyn Fn(Option<&str>) -> Option<MainTreeSovereigntyContext> + Send + Sync>;

/// Host-wired provider for the #20b guard. Called once per shell execution
/// with the calling session's goal id; returns `None` to disable the guard
/// entirely (default). Wired by the serve/runtime layer (octos-cli) where the
/// profile data dir and workspace root are known; process-global because tool
/// construction sites vastly outnumber the single boot that knows both paths.
static MAIN_TREE_SOVEREIGNTY_PROVIDER: std::sync::RwLock<Option<MainTreeSovereigntyProvider>> =
    std::sync::RwLock::new(None);

/// Install (or clear, with `None`) the #20b main-tree sovereignty provider.
pub fn set_main_tree_sovereignty_provider(provider: Option<MainTreeSovereigntyProvider>) {
    let mut slot = MAIN_TREE_SOVEREIGNTY_PROVIDER
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = provider;
}

/// Snapshot the currently-installed #20b sovereignty provider, if any. The
/// shell tool itself reads the slot on the execution path; this accessor
/// exists for host-side wiring tests (octos-cli #20c) that must invoke the
/// INSTALLED provider — not a re-implemented closure — to prove the
/// scan/claim/deny wiring end to end.
pub fn main_tree_sovereignty_provider() -> Option<MainTreeSovereigntyProvider> {
    MAIN_TREE_SOVEREIGNTY_PROVIDER
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// The HEAD-moving target of a `git checkout` / `git switch` segment:
/// `Some(Some(branch))` switches to / creates `branch`, `Some(None)` is a
/// HEAD-moving form with no branch target we can name (treated as
/// switching), and `None` is anything else (read-only, pathspec restore,
/// non-checkout git). Reused segment tokenizer honours quotes/`;`/`&&`/`|`.
fn git_head_move_target(segment: &[String], cwd: &Path) -> Option<Option<String>> {
    let mut i = 0;
    // Walk to the `git` token, skipping env assignments and wrappers exactly
    // like `segment_invokes_git`.
    loop {
        let token = segment.get(i)?;
        if token_invokes_git(token) {
            i += 1;
            break;
        }
        if looks_like_env_assignment(token) {
            i += 1;
            continue;
        }
        if is_git_invocation_wrapper(token) {
            i += 1;
            while segment
                .get(i)
                .is_some_and(|w| w.starts_with('-') || looks_like_env_assignment(w))
            {
                i += 1;
            }
            continue;
        }
        return None;
    }
    // Skip git global options; `-C <path>` / `-c <kv>` consume a value.
    while let Some(token) = segment.get(i) {
        match token.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" => i += 2,
            _ if token.starts_with('-') => i += 1,
            _ => break,
        }
    }
    let sub = segment.get(i)?;
    let is_checkout = sub.eq_ignore_ascii_case("checkout");
    let is_switch = sub.eq_ignore_ascii_case("switch");
    if !is_checkout && !is_switch {
        return None;
    }
    i += 1;
    let mut positional: Option<&str> = None;
    let mut after_ddash = false;
    while let Some(token) = segment.get(i) {
        if after_ddash {
            // Everything after `--` is pathspec — restoring files, never a
            // HEAD move (for the forms we intercept).
            return None;
        }
        match token.as_str() {
            "--" => after_ddash = true,
            // Branch-creating forms: HEAD moves to the NEW branch.
            "-b" | "-B" | "-c" | "-C" | "--orphan" => {
                let name = segment.get(i + 1);
                return Some(name.map(|s| s.to_owned()).or(Some(String::new())));
            }
            // Value-taking options we must skip over.
            "--conflict" | "--pathspec-from-file" | "-m" | "--merge" => i += 2,
            "-d" | "--detach" => {
                // Detached HEAD: moves HEAD off any branch. The next token is
                // the ref; a bare detach of the current commit is harmless,
                // but conservatively treat as a HEAD move with unknown target.
                return Some(
                    segment
                        .get(i + 1)
                        .map(|s| s.to_owned())
                        .or(Some(String::new())),
                );
            }
            _ if token.starts_with('-') => i += 1,
            _ => {
                positional = Some(token.as_str());
                break;
            }
        }
        i += 1;
    }
    let target = positional?;
    // `git checkout <path>` (no `--`) is a pathspec restore when the target
    // exists as a filesystem entry under cwd — git itself prefers the branch
    // reading, but refusing a file restore would be a false positive on the
    // common `git checkout some_file` idiom, so only treat as a switch when
    // no such path exists.
    if cwd.join(target).exists() {
        return None;
    }
    Some(Some(target.to_owned()))
}

/// Pure #20b decision: `Some(denial)` when this command must be refused.
/// `pub` so the octos-cli #20c dual-goal fixture can exercise the real
/// decision against a real provider-shaped context (cross-goal checkout
/// refused, owner goal passes through) instead of a re-implemented copy.
pub fn tree_sovereignty_denial(
    command: &str,
    effective_cwd: &Path,
    ctx: &MainTreeSovereigntyContext,
) -> Option<String> {
    // Main-tree scope only: a fenced peer's clone is a DIFFERENT directory.
    let cwd = octos_core::session_scope::canonicalize_lossy(effective_cwd);
    let root = octos_core::session_scope::canonicalize_lossy(&ctx.main_tree_root);
    if cwd != root {
        return None;
    }
    let branch = ctx.main_tree_branch.as_deref()?;
    let owner = ctx.owner_goal_id.as_deref()?;
    if ctx.caller_goal_id.as_deref() == Some(owner) {
        return None;
    }
    for segment in shell_command_segments(command) {
        let Some(target) = git_head_move_target(&segment, effective_cwd) else {
            continue;
        };
        // Switching TO the current branch (or `checkout -b` naming it) is a
        // no-op on tree state — allow.
        if target.as_deref() == Some(branch) {
            continue;
        }
        let target_label = target.as_deref().unwrap_or("(detached/unknown)");
        let caller = ctx.caller_goal_id.as_deref().unwrap_or("(no goal)");
        return Some(format!(
            "Command refused by main-tree sovereignty (#20b): the main tree is owned by goal \
             '{owner}' and currently on branch '{branch}', but this session (goal: {caller}) tried \
             to switch it to '{target_label}'. The tree was NOT switched. To work on another \
             branch, fence yourself instead: ask the master to stage a peer (peer_handoff auto-\
             fences onto a worktree clone under peers/<slug>/wt), or run this work inside your own \
             worktree — never `git checkout` the shared main tree across branches."
        ));
    }
    None
}

fn shell_command_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut segment = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        if let Some(quote_ch) = quote {
            if ch == quote_ch {
                quote = None;
            } else if quote_ch == '"' && ch == '\\' {
                if let Some(&next) = chars.peek() {
                    if matches!(next, '"' | '\\' | '$' | '`' | '\n') {
                        token.push(chars.next().expect("peeked char exists"));
                    } else {
                        token.push(ch);
                    }
                } else {
                    token.push(ch);
                }
            } else {
                token.push(ch);
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' => {
                if let Some(&next) = chars.peek() {
                    if is_shell_token_separator(next) || matches!(next, '\'' | '"' | '\\') {
                        token.push(chars.next().expect("peeked char exists"));
                    } else {
                        token.push(ch);
                    }
                } else {
                    token.push(ch);
                }
            }
            '\n' | ';' | '&' | '|' | '(' | ')' => {
                push_shell_token(&mut segment, &mut token);
                push_shell_segment(&mut segments, &mut segment);
            }
            ch if ch.is_whitespace() => push_shell_token(&mut segment, &mut token),
            _ => token.push(ch),
        }
    }

    push_shell_token(&mut segment, &mut token);
    push_shell_segment(&mut segments, &mut segment);
    segments
}

fn is_shell_token_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, ';' | '&' | '|' | '(' | ')')
}

fn push_shell_token(segment: &mut Vec<String>, token: &mut String) {
    if !token.is_empty() {
        segment.push(std::mem::take(token));
    }
}

fn push_shell_segment(segments: &mut Vec<Vec<String>>, segment: &mut Vec<String>) {
    if !segment.is_empty() {
        segments.push(std::mem::take(segment));
    }
}

fn segment_invokes_git(segment: &[String]) -> bool {
    let mut index = 0;
    while let Some(token) = segment.get(index) {
        if token_invokes_git(token) {
            return true;
        }
        if looks_like_env_assignment(token) {
            index += 1;
            continue;
        }
        if is_git_invocation_wrapper(token) {
            index += 1;
            while segment.get(index).is_some_and(|wrapped| {
                wrapped.starts_with('-') || looks_like_env_assignment(wrapped)
            }) {
                index += 1;
            }
            continue;
        }
        return false;
    }
    false
}

fn token_invokes_git(token: &str) -> bool {
    if looks_like_env_assignment(token) {
        return false;
    }
    let basename = command_basename(token);
    basename.eq_ignore_ascii_case("git") || basename.eq_ignore_ascii_case("git.exe")
}

fn is_git_invocation_wrapper(token: &str) -> bool {
    const WRAPPERS: &[&str] = &[
        "env",
        "env.exe",
        "time",
        "time.exe",
        "sudo",
        "sudo.exe",
        "npx",
        "npx.exe",
        "command",
        "command.exe",
        "exec",
        "exec.exe",
    ];

    let basename = command_basename(token);
    WRAPPERS
        .iter()
        .any(|wrapper| basename.eq_ignore_ascii_case(wrapper))
}

fn command_basename(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

fn looks_like_env_assignment(token: &str) -> bool {
    let Some((name, _value)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn apply_git_tool_env(cmd: &mut tokio::process::Command, command: &str) {
    if contains_git_invocation(command) {
        cmd.env("GIT_CONFIG_GLOBAL", NULL_DEVICE_PATH)
            .env("GIT_CONFIG_NOSYSTEM", "1");
    }
}

fn apply_harness_event_sink_env(cmd: &mut tokio::process::Command, ctx: &ToolContext) {
    apply_continuation_owner_env(cmd, ctx);
    if let Some(sink) = ctx.harness_event_sink.as_deref() {
        cmd.env("OCTOS_EVENT_SINK", sink);
        return;
    }
    // Legacy callers that route through `execute()` pass `ToolContext::zero()` —
    // the sink isn't on the typed context but may still live on the
    // task-local `TOOL_CTX` that older executor paths populate.
    if let Ok(Some(sink)) = TOOL_CTX.try_with(|inner| inner.harness_event_sink.clone()) {
        cmd.env("OCTOS_EVENT_SINK", sink);
    }
}

/// Native CLI helpers launched by an agent need the same evidence identity
/// as plugin calls. Never inherit a process-wide identity from another session.
pub(super) fn apply_continuation_owner_env(cmd: &mut tokio::process::Command, ctx: &ToolContext) {
    cmd.env_remove("OCTOS_CONTINUATION_OWNER");
    let owner = ctx
        .continuation_owner
        .clone()
        .or_else(|| {
            TOOL_CTX
                .try_with(|ctx| ctx.continuation_owner.clone())
                .ok()
                .flatten()
        })
        .or_else(|| super::CONTINUATION_OWNER.try_with(Clone::clone).ok());
    if let Some(owner) = owner {
        cmd.env("OCTOS_CONTINUATION_OWNER", owner);
    }
}

/// Outer-loop #4 (docs/build-cache-pool.md §7.4): inject the build-cache
/// pool slot's cargo env for a peer turn. `CARGO_TARGET_DIR=<slot>/target`
/// redirects every cargo invocation into the shared per-repository pool slot
/// the peer's CURRENT turn holds, and `CARGO_INCREMENTAL=0` is sent
/// explicitly even when a machine-level `~/.cargo/config.toml` already sets
/// it — the peer's CARGO_HOME is not under our control and the #1 incident
/// measured 31 GB of incremental artifacts across 515 sessions.
///
/// PER TOOL CALL on purpose: on the serve path a peer shares the process
/// with the master and every sibling peer, so `std::env::set_var` here would
/// poison all of them. The slot rides `ToolContext.build_cache_slot`
/// (populated by the peer turn boot), falling back to the task-local
/// `TOOL_CTX` for legacy `execute()` callers — the same dual-read pattern as
/// [`apply_harness_event_sink_env`].
///
/// Ordering vs `sanitize_command_env` (which runs before this in both the
/// foreground and background paths): the sanitizer only strips
/// secret/injection names (`BLOCKED_ENV_VARS`, registered provider keys,
/// secret-looking names — see `subprocess_env.rs` / `env_hygiene.rs`), and
/// neither `CARGO_TARGET_DIR` nor `CARGO_INCREMENTAL` is in that set, so a
/// value set here survives; setting it AFTER the sanitizer keeps that
/// property obvious rather than incidental.
fn resolve_build_cache_slot(ctx: &ToolContext) -> Option<PathBuf> {
    ctx.build_cache_slot.clone().or_else(|| {
        TOOL_CTX
            .try_with(|inner| inner.build_cache_slot.clone())
            .ok()
            .flatten()
    })
}

fn begin_build_cache_use(
    ctx: &ToolContext,
) -> Result<Option<super::BuildCacheUseGuard>, &'static str> {
    let usage = if ctx.build_cache_slot.is_some() || ctx.build_cache_usage.is_some() {
        ctx.build_cache_usage.clone()
    } else {
        TOOL_CTX
            .try_with(|inner| inner.build_cache_usage.clone())
            .ok()
            .flatten()
    };
    match usage {
        Some(usage) => usage
            .begin()
            .map(Some)
            .ok_or("Build-cache claim is closing; shell command was not started"),
        None => Ok(None),
    }
}

/// Use the SAME resolved option as the sandbox wrapper for this call.
fn apply_build_cache_env(cmd: &mut tokio::process::Command, slot: Option<&Path>) {
    let Some(slot) = slot else {
        return;
    };
    cmd.env("CARGO_TARGET_DIR", slot.join("target"))
        .env("CARGO_INCREMENTAL", "0");
}

#[derive(Debug, Deserialize)]
// #1770: unknown keys are usually a typo of a real parameter; rejecting
// them (with a did-you-mean via `args::parse_tool_args`) lets the model
// self-correct instead of silently dropping its intent.
#[serde(deny_unknown_fields)]
struct ShellInput {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Explicit request to run the command detached (in addition to the
    /// trailing-`&` heuristic). When true, the command is registered as a
    /// tracked background task and control returns immediately.
    #[serde(default)]
    background: Option<bool>,
}

/// True when the shell command should run in the background: either an
/// explicit `background: true` arg, or a trailing `&` (the command backgrounds
/// itself). A trailing `&&` is the logical-AND operator, not a background
/// request, so it is excluded.
fn is_background_command(command: &str, explicit: Option<bool>) -> bool {
    explicit == Some(true) || has_trailing_ampersand(command)
}

/// True when `command` ends with a single background `&` (ignoring trailing
/// whitespace) that is not part of a `&&` operator.
fn has_trailing_ampersand(command: &str) -> bool {
    let trimmed = command.trim_end();
    trimmed.ends_with('&') && !trimmed.ends_with("&&")
}

/// Strip a single trailing `&` (and surrounding whitespace) so the command
/// runs as OUR detached child rather than being re-backgrounded by the wrapper
/// shell — which would `fork` the real work, exit immediately, and orphan the
/// grandchild (reparented to PID 1), defeating lifecycle tracking. With the
/// `&` removed, our `sh -c "<cmd>"` child IS the work, so the watcher's
/// `child.wait()` observes the real completion.
fn strip_trailing_ampersand(command: &str) -> String {
    let trimmed = command.trim_end();
    if let Some(stripped) = trimmed.strip_suffix('&') {
        let stripped = stripped.trim_end();
        // Guard against `&&` (invalid at end anyway): only strip a lone `&`.
        if !stripped.ends_with('&') {
            return stripped.to_string();
        }
    }
    command.to_string()
}

/// Build a short, single-line human label for a background command, used as
/// the supervisor task's display name in `/ps`. Mirrors how the spawn tool
/// passes a human label as the registered task's `tool_name`.
fn background_label(command: &str) -> String {
    let one_line: String = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut label = String::from("shell: ");
    const MAX: usize = 80;
    if one_line.chars().count() > MAX {
        label.extend(one_line.chars().take(MAX));
        label.push('…');
    } else {
        label.push_str(&one_line);
    }
    label
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command. Truncated output includes a full log path for read_file/grep."
    }

    fn tags(&self) -> &[&str] {
        &["runtime", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        // Shell commands can mutate the filesystem or spawn long-lived
        // processes. Running them in parallel with other tool calls races
        // observable state (e.g. `shell: rm foo` vs `read_file foo/x`), so
        // shell runs in the serialized Exclusive phase — after every Safe
        // sibling in the batch has completed. See M8.8 and #1766.
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Optional timeout in seconds (default: 120)"
                },
                "background": {
                    "type": "boolean",
                    "description": "Run the command detached in the background. A trailing `&` also backgrounds the command. Background commands are tracked as tasks (visible in `/ps`) and return immediately."
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        // Legacy entry point: route through the typed path with a zero-value
        // context so out-of-band callers (tests, `ToolRegistry::execute`)
        // exercise the same Phase 2-D scope resolution as migrated callers.
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let input: ShellInput =
            super::args::parse_tool_args(self.name(), &self.input_schema(), args)?;

        // Phase 2-D of the SessionScope migration: when the host has
        // threaded a scope through `ToolContext`, prefer the scope's
        // workspace so every tool in the session shares the single
        // filesystem contract.
        //
        // Codex P1 (round-1, this PR): respect a hinted workspace
        // override. `SessionRuntime` builds `session_scope` from
        // `<data_dir>/users/<id>/workspace` independent of any
        // `workspace_hint` supplied by the coding-agent flow, while
        // the registry rebinds every tool's `cwd` to the *hinted*
        // workspace via `with_workspace_root`. If `self.cwd` differs
        // from `scope.workspace()`, the caller deliberately pointed
        // this tool at a different workspace — honour that and keep
        // `self.cwd`. Otherwise the migration is a no-op for the
        // hinted-coding-agent path until Phase 3 reconciles
        // SessionScope construction with the hinted workspace.
        //
        // The same effective CWD also feeds the `CommandPolicy::check`
        // call so a policy that consults the working directory sees a
        // consistent value with what the child process will observe.
        let effective_cwd: &Path = match ctx.session_scope.as_ref() {
            Some(scope) if scope.workspace() == self.cwd.as_path() => scope.workspace(),
            _ => &self.cwd,
        };
        // #28b — deny knob: heuristic pre-screen of the command TEXT for
        // write-shaped shell usage. False negatives are tolerated (the 28a
        // receipt still shows what actually changed); false positives
        // escape via a trailing `# octos:allow-write` comment on the
        // command line (documented escape hatch). The knob was loaded at
        // construction — no per-call I/O.
        if self.bash_file_writes == BashFileWrites::Deny
            && !command_allows_write_explicitly(&input.command)
            && command_looks_like_file_write(&input.command)
        {
            return Ok(ToolResult {
                output: "Command refused by tool_policy.bash_file_writes=deny (it looks like a file-writing shell command). Use the edit_file / diff_edit tools for code changes instead. If this refusal is a false positive, append the comment `# octos:allow-write` to the command line to run it explicitly. Command: ".to_owned() + &input.command,
                success: false,
                ..Default::default()
            });
        }
        // #28a — BEFORE snapshot for the file-change receipt (git-status
        // level; None on non-git/fail-open omits the receipt entirely).
        let dirty_before = snapshot_dirty_paths(effective_cwd);

        // #20b — main-tree sovereignty: refuse a cross-branch `git checkout`
        // / `git switch` on the owned main tree BEFORE policy, approval, or
        // any execution path (foreground or background). Fail-open when no
        // provider is wired, no owner is recorded, or the branch is unknown.
        if let Some(provider) = MAIN_TREE_SOVEREIGNTY_PROVIDER
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            if let Some(sovereignty) = provider(ctx.goal_id.as_deref()) {
                if let Some(denial) =
                    tree_sovereignty_denial(&input.command, effective_cwd, &sovereignty)
                {
                    tracing::warn!(command = %input.command, "command refused by main-tree sovereignty");
                    return Ok(ToolResult {
                        output: format!("{denial}\n\nCommand: {}", input.command),
                        success: false,
                        ..Default::default()
                    });
                }
            }
        }

        // Check policy first
        let decision = self.policy.check(&input.command, effective_cwd);
        match decision {
            Decision::Deny => {
                tracing::warn!(command = %input.command, "command denied by policy");
                return Ok(ToolResult {
                    output: format!(
                        "Command denied by security policy: {}\n\nThis command was blocked because it matches a dangerous pattern.",
                        input.command
                    ),
                    success: false,
                    ..Default::default()
                });
            }
            Decision::Ask => {
                if !self.approval_policy.allows_prompt() {
                    tracing::warn!(
                        command = %input.command,
                        "command requires approval but approval policy is never"
                    );
                    return Ok(ToolResult {
                        output: format!(
                            "Command requires approval but approval_policy is never: {}",
                            input.command
                        ),
                        success: false,
                        ..Default::default()
                    });
                }

                let requester = TOOL_APPROVAL_CTX.try_with(Clone::clone).ok();
                let Some(requester) = requester else {
                    tracing::warn!(command = %input.command, "command requires approval — denied (no interactive approval available)");
                    return Ok(ToolResult {
                        output: format!(
                            "Command requires approval and was denied: {}\n\nThis command matches a potentially dangerous pattern (e.g. sudo, rm -rf, git push --force). It cannot be executed without interactive approval.",
                            input.command
                        ),
                        success: false,
                        ..Default::default()
                    });
                };

                let tool_id = if ctx.tool_id.is_empty() {
                    TOOL_CTX
                        .try_with(|inner| inner.tool_id.clone())
                        .unwrap_or_default()
                } else {
                    ctx.tool_id.clone()
                };
                let decision = requester
                    .request_approval(ToolApprovalRequest {
                        tool_id,
                        tool_name: self.name().to_owned(),
                        title: "Approve shell command".to_owned(),
                        body: format!("Run command: {}", input.command),
                        command: Some(input.command.clone()),
                        cwd: Some(effective_cwd.to_string_lossy().into_owned()),
                        once_only: false,
                        host_tool: None,
                    })
                    .await;
                if matches!(decision, ToolApprovalDecision::Deny) {
                    tracing::warn!(command = %input.command, "command denied by interactive approval");
                    return Ok(ToolResult {
                        output: format!("Command denied by user approval: {}", input.command),
                        success: false,
                        ..Default::default()
                    });
                }
            }
            Decision::Allow => {}
        }

        // A closed worker (`background_allowed == false`) refuses the two
        // string-detectable ways to detach: the explicit `background: true`
        // arg and a trailing `&` (checked before the background branch below,
        // so neither falls through to a foreground run that self-detaches).
        // This is best-effort defense-in-depth, NOT a boundary: arbitrary
        // shell-internal backgrounding (`sleep 600 & true`, `sh -c "cmd &"`)
        // cannot be caught by string inspection — the SANDBOX's process-group
        // teardown is what actually bounds a detached child.
        if is_background_command(&input.command, input.background) && !self.background_allowed {
            tracing::warn!(
                command = %input.command,
                "background/detached shell execution is disabled for this worker",
            );
            return Ok(ToolResult {
                output: format!(
                    "background/detached execution is disabled for this worker: {}",
                    input.command
                ),
                success: false,
                ..Default::default()
            });
        }

        // Background execution: a trailing `&` or an explicit `background: true`
        // arg asks the command to run detached. Register it as a tracked
        // supervisor task (so it surfaces in `/ps` and the sub-agent dock),
        // spawn it detached through the same sandbox, and return immediately
        // without waiting. Foreground commands (no `&`) are unchanged.
        // Fail closed on an unhonorable sandbox config BEFORE spawning
        // anything: the typed refusal (model-facing Display — operator
        // remediation stays in the logs) IS the tool result. The wrap-level
        // refusal command would also fail, but a background command discards
        // its stderr, and the model deserves the full refusal text, not a
        // truncated stderr line.
        if let Some(refusal) = self.sandbox.refusal() {
            return Ok(ToolResult {
                output: refusal.to_string(),
                success: false,
                ..Default::default()
            });
        }

        if is_background_command(&input.command, input.background) {
            return Ok(self
                .execute_background(ctx, &input.command, effective_cwd)
                .await);
        }

        // Clamp timeout to [1, 600] seconds to prevent abuse
        const MIN_TIMEOUT: u64 = 1;
        const MAX_TIMEOUT: u64 = 600;
        let mut timeout_duration = input
            .timeout_secs
            .map(|s| Duration::from_secs(s.clamp(MIN_TIMEOUT, MAX_TIMEOUT)))
            .unwrap_or(self.timeout);
        // Apply the hard per-command ceiling (if configured): the effective
        // timeout can only be LOWERED by it, never raised. This is what keeps a
        // closed worker's foreground command bounded by its attempt deadline
        // even when the LLM requests a larger `timeout_secs`.
        if let Some(cap) = self.max_timeout {
            timeout_duration = timeout_duration.min(cap);
        }

        // Execute command (through sandbox).
        // Spawn the child, grab its PID, then timeout on wait_with_output().
        // If timeout fires, kill by PID to prevent orphaned processes.
        // (wait_with_output() takes ownership of child, so we save the PID first.)
        let usage_guard = match begin_build_cache_use(ctx) {
            Ok(guard) => guard,
            Err(message) => {
                return Ok(ToolResult {
                    output: message.to_owned(),
                    success: false,
                    ..Default::default()
                });
            }
        };
        let slot = resolve_build_cache_slot(ctx);
        let mut cmd = self.sandbox.wrap_command_with_build_cache_slot(
            &input.command,
            effective_cwd,
            slot.as_deref(),
        );
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        apply_frontend_tool_env(&mut cmd, effective_cwd);
        apply_quarto_tool_env(&mut cmd, &input.command, effective_cwd);
        apply_git_tool_env(&mut cmd, &input.command);
        sanitize_command_env(&mut cmd, &EnvAllowlist::empty());
        apply_build_cache_env(&mut cmd, slot.as_deref());
        apply_harness_event_sink_env(&mut cmd, ctx);

        let capture = super::command_output::CommandOutput::new(effective_cwd)?;
        // T-0241: put the child in its OWN process group so the terminate
        // path (turn cancel AND the pre-existing timeout kill) can signal
        // the whole tree — `kill -- -<pid>` requires the child to be a
        // group leader; without `process_group(0)` the negative-PID signal
        // silently missed backgrounded grandchildren. Sandboxed wraps
        // (bwrap/landlock helper) keep the same group, so the signal still
        // reaches every confined process; the Docker client forwards
        // SIGTERM to the container (`docker run --rm` cleans up), and the
        // SIGKILL fallback is documented as client-side only.
        #[cfg(unix)]
        cmd.process_group(0);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Failed to execute command: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };
        let child_pid = child.id();

        // The waiter owns both the child and the claim usage. Dropping the
        // caller's future detaches this waiter, so release still waits for exit.
        let captured = capture.clone();
        let waiter = tokio::spawn(async move {
            let _usage_guard = usage_guard;
            super::command_output::wait_with_capture(child, captured).await
        });
        let timed = timeout(timeout_duration, async {
            waiter.await.map_err(std::io::Error::other)?
        });
        // T-0241: race the wait against the turn cancel token. On cancel we
        // terminate the process group exactly like the timeout arm and
        // return an honest cancelled result; the batch layer's grace
        // (TOOL_TURN_CANCEL_GRACE) is sized to let this finish first.
        let turn_cancelled = ctx.turn_cancel.clone();
        let result = match turn_cancelled {
            Some(token) => {
                tokio::select! {
                    r = timed => Some(r),
                    _ = token.cancelled() => {
                        terminate_process_tree(child_pid).await;
                        None
                    }
                }
            }
            None => Some(timed.await),
        };
        if result.is_none() {
            capture.record_error("command cancelled by turn interrupt");
            let mut view = capture.view(false);
            return Ok(ToolResult {
                output: format!(
                    "Command cancelled: the turn was interrupted (Stop / turn/interrupt / work-secret revocation). The process group was terminated (SIGTERM, then SIGKILL after a grace period).\n{}",
                    view.render(octos_core::tool_output_limit(self.name()).saturating_sub(2048))
                ),
                success: false,
                structured_metadata: Some(view.metadata()),
                ..Default::default()
            });
        }
        let result = result.unwrap_or_else(|| unreachable!("checked above"));

        match result {
            Ok(Ok(output)) => {
                let mut view = capture.view(false);
                let exit_code = output.code().unwrap_or(-1);
                let mut result_text =
                    view.render(octos_core::tool_output_limit(self.name()).saturating_sub(2048));
                if result_text.is_empty() {
                    result_text = "(no output)".to_string();
                }

                // Scan the retained output tail for sandbox denials. Earlier
                // diagnostics remain available through the full log.
                let denial_hint = crate::sandbox::sandbox_denial_hint(
                    !self.sandbox.is_noop(),
                    output.success(),
                    &result_text,
                );

                // Truncate if too long (reserve space for exit code suffix)
                let exit_suffix = format!("\n\nExit code: {exit_code}");
                let max_output = octos_core::tool_output_limit(self.name());
                octos_core::truncate_utf8(
                    &mut result_text,
                    max_output - exit_suffix.len(),
                    "\n... (output truncated)",
                );

                result_text.push_str(&exit_suffix);
                if let Some(hint) = denial_hint {
                    result_text.push_str(hint);
                }

                // #28a — file-change receipt: AFTER snapshot + diff,
                // appended ONCE to THIS result's tail (never the system
                // prompt, never a history rewrite — prompt-cache stable).
                let receipt = diff_to_receipt(dirty_before, snapshot_dirty_paths(effective_cwd));
                if let Some(receipt) = receipt.as_deref() {
                    result_text.push_str(receipt);
                    // #28b — warn knob: nudge ONLY when files actually
                    // changed (files_changed > 0), sharing the receipt's
                    // AFTER snapshot (no second git-status scan). Zero
                    // behavior change under `allow`.
                    if self.bash_file_writes == BashFileWrites::Warn
                        && !receipt.trim_end().ends_with("files_changed: 0")
                    {
                        result_text.push_str(
                            "\nnote: prefer the edit_file / diff_edit tools for code changes (tool_policy.bash_file_writes=warn)",
                        );
                    }
                }

                Ok(ToolResult {
                    output: result_text,
                    success: output.success(),
                    structured_metadata: Some(view.metadata()),
                    ..Default::default()
                })
            }
            Ok(Err(e)) => {
                capture.record_error(&e);
                let mut view = capture.view(false);
                Ok(ToolResult {
                    output: format!(
                        "Failed to execute command: {e}\n{}",
                        view.render(
                            octos_core::tool_output_limit(self.name()).saturating_sub(2048)
                        )
                    ),
                    success: false,
                    structured_metadata: Some(view.metadata()),
                    ..Default::default()
                })
            }
            Err(_) => {
                terminate_process_tree(child_pid).await;
                capture.record_error("command timed out; output may be incomplete");
                let mut view = capture.view(false);
                Ok(ToolResult {
                    output: format!(
                        "Command timed out after {} seconds\n{}\nFull output: {}",
                        timeout_duration.as_secs(),
                        view.render(
                            octos_core::tool_output_limit(self.name()).saturating_sub(2048)
                        ),
                        view.path.display()
                    ),
                    success: false,
                    structured_metadata: Some(view.metadata()),
                    ..Default::default()
                })
            }
        }
    }
}

/// Graceful shutdown shared by the timeout arm and the T-0241 turn-cancel
/// arm: SIGTERM first, then SIGKILL after a grace period.
/// `wait_with_capture()` consumed the Child, so we kill via PID. The
/// negative PID targets the entire process group — real because the
/// foreground spawn installs `process_group(0)` (T-0241); the direct-PID
/// signal remains as a fallback for a child that re-grouped itself.
async fn terminate_process_tree(child_pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = child_pid {
        use std::process::Command as StdCommand;

        // 1. Send SIGTERM to process group for graceful shutdown.
        // `--` is required before the negative PID: GNU/procps
        // `kill` otherwise parses `-<pid>` as an option and the
        // group signal is silently never delivered (macOS
        // accepted the bare form, Linux did not).
        let group = format!("-{pid}");
        let _ = StdCommand::new("kill").args(["-15", "--", &group]).status();
        let _ = StdCommand::new("kill")
            .args(["-15", &pid.to_string()])
            .status();

        // 2. Brief grace period, then SIGKILL gated on a probe of
        // the GROUP — a leader-only probe skipped the escalation
        // when the shell died to SIGTERM while backgrounded
        // grandchildren lived on (#1781 CI). `kill -0 -- -pgid`
        // succeeds while ANY member is alive and cannot hit a
        // recycled group while a member remains.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let group_alive = StdCommand::new("kill")
            .args(["-0", "--", &group])
            .status()
            .is_ok_and(|s| s.success());
        if group_alive {
            let _ = StdCommand::new("kill").args(["-9", "--", &group]).status();
        }
        let leader_alive = StdCommand::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success());
        if leader_alive {
            let _ = StdCommand::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
    }
    #[cfg(windows)]
    if let Some(pid) = child_pid {
        use std::process::Command as StdCommand;
        // `/T` kills the whole process tree — the Windows equivalent of the
        // Unix process-group signal. There is no graceful-then-forced two
        // step without extra Win32 plumbing; `/F` is documented behavior.
        let _ = StdCommand::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .status();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::ShellTool;
    use crate::tools::{Tool, ToolContext};

    // ------------------------------------------------------------------
    // T-0241: turn cancellation terminates the whole process group.
    // ------------------------------------------------------------------

    /// Build a ctx whose turn token is the given one.
    fn ctx_with_turn_cancel(
        token: tokio_util::sync::CancellationToken,
    ) -> crate::tools::ToolContext {
        crate::tools::ToolContext {
            turn_cancel: Some(token),
            ..crate::tools::ToolContext::zero()
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn should_terminate_process_group_when_turn_cancelled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("grandchild-wrote-this");
        // Unique cmdline token so we can prove no stray process survives.
        let uniq = format!("t0241cancel{}", std::process::id());
        // The backgrounded `( sleep 1; touch marker )` is a grandchild in
        // the same process group; the leader sleeps 30s. A cancel must kill
        // BOTH before the grandchild's write lands.
        let command = format!(
            "( sleep 1; touch {marker} ) & sleep 30 # {uniq}",
            marker = marker.display(),
        );
        let tool = super::ShellTool::new(temp.path());
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = ctx_with_turn_cancel(token.clone());
        let args = serde_json::json!({"command": command, "timeout_secs": 60});

        let exec = tokio::spawn(async move { tool.execute_with_context(&ctx, &args).await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        token.cancel();
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), exec)
            .await
            .expect("cancelled shell must return within 5s")
            .expect("join")
            .expect("execute");
        assert!(!out.success, "cancelled command must fail the tool call");
        assert!(
            out.output.contains("cancelled") && out.output.contains("process group"),
            "honest cancelled text: {}",
            out.output
        );
        // The grandchild was due to write at t=1s; wait past it, then prove
        // the write never landed and no group member survives.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "cancelled shell's grandchild must not finish its write"
        );
        let pgrep = std::process::Command::new("pgrep")
            .args(["-f", &uniq])
            .output()
            .expect("pgrep");
        assert!(
            !pgrep.status.success(),
            "no process may survive the cancel: {}",
            String::from_utf8_lossy(&pgrep.stdout)
        );
    }

    #[cfg(unix)]
    #[test]
    fn bash_file_writes_deny_refuses_write_command() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = super::ShellTool::new(temp.path())
            .with_bash_file_writes(crate::tools::policy::BashFileWrites::Deny);
        let out = tokio::runtime::Runtime::new()
            .expect("rt")
            .block_on(tool.execute(&serde_json::json!({
                "command": "echo hi > /tmp/never-created-28b",
            })))
            .expect("execute");
        assert!(!out.success);
        assert!(out.output.contains("tool_policy.bash_file_writes=deny"));
        assert!(out.output.contains("edit_file / diff_edit"));
        assert!(out.output.contains("# octos:allow-write"));
        assert!(!std::path::Path::new("/tmp/never-created-28b").exists());
        std::fs::remove_file("/tmp/never-created-28b").ok();
    }

    // -----------------------------------------------------------------------
    // Phase 2-D: SessionScope integration tests for ShellTool.
    //
    // The child process CWD is the load-bearing observable here — a shell
    // command that runs `pwd` (or the `cd` cmd-builtin equivalent on
    // Windows) must see `scope.workspace()` when the host has threaded a
    // scope through `ToolContext`, and must see `self.cwd` (legacy
    // behaviour) when the host has not.
    // -----------------------------------------------------------------------

    fn ctx_with_scope(scope: octos_core::SessionScope) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "shell-with-scope".to_string();
        ctx.session_scope = Some(Arc::new(scope));
        ctx
    }

    #[tokio::test]
    async fn max_timeout_secs_caps_a_larger_requested_timeout() {
        // A hard 1s ceiling must override a larger requested `timeout_secs`:
        // `sleep 30` with `timeout_secs: 30` under `with_max_timeout_secs(1)`
        // is killed at ~1s, so a closed worker's foreground command cannot
        // outlive its deadline even when the LLM asks for a big timeout.
        let tool = ShellTool::new(std::env::temp_dir()).with_max_timeout_secs(1);
        let started = std::time::Instant::now();
        let result = tool
            .execute(&serde_json::json!({"command": "sleep 30", "timeout_secs": 30}))
            .await
            .unwrap();
        assert!(!result.success, "a capped, timed-out command must fail");
        assert!(
            result.output.contains("timed out after 1 seconds"),
            "expected a 1s timeout (cap applied), got: {}",
            result.output
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the 1s cap must bound the wait well under the requested 30s",
        );
    }

    #[tokio::test]
    async fn shell_safe_policy_still_denies_with_scope_present() {
        // Codex-anticipated regression: a scope on the context must NOT
        // weaken the `SafePolicy` denylist — `rm -rf /` is refused
        // whether or not a scope is set. The CWD that the policy sees
        // changes (it now sees the scope workspace), but the denylist is
        // command-string only so the verdict is unchanged.
        let scope_dir = tempfile::tempdir().unwrap();
        let scope = octos_core::SessionScope::solo(scope_dir.path().to_path_buf(), vec![])
            .expect("scope construction");
        let tool = ShellTool::new(std::env::temp_dir());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"command": "rm -rf /"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("denied"),
            "expected deny, got: {}",
            result.output
        );
    }
}

// ---------------------------------------------------------------------------
// #28b — bash file-writes knob helpers.
// ---------------------------------------------------------------------------

/// #28b — escape hatch: a trailing `# octos:allow-write` comment on the
/// command line explicitly authorizes a write-shaped command under `deny`.
pub(crate) fn command_allows_write_explicitly(command: &str) -> bool {
    command
        .lines()
        .last()
        .is_some_and(|last| last.contains("# octos:allow-write"))
}

/// #28b — heuristic: does this command LOOK like it writes files? A
/// curated WHITELIST of shapes (documented; false negatives tolerated —
/// the 28a receipt still reports what actually changed):
///   * shell redirection to a file: `> file`, `>> file`, `2> file`, `&> file`
///   * `tee file` / `tee -a file`
///   * in-place editors: `sed -i`, `gawk -i`, `perl -pi`, `ruby -pi`
///   * `cp`/`mv`/`rm`/`mkdir`/`touch`/`truncate`/`ln` with a non-flag arg
///   * heredocs: `<<EOF`, `<<-EOF`, `<<'EOF'`, `<<"EOF"`
///   * `python -c`/`python3 -c`/`node -e` whose payload mentions
///     open(...,"w") / fs.write — the two dominant scripted-write shapes
///   * `dd of=`, `install`, `patch <`, `git apply`, `unzip -o`, `tar -x`
pub(crate) fn command_looks_like_file_write(command: &str) -> bool {
    // Redirections (single-line scan; quotes rarely wrap the target).
    for token_hint in [">>", " 2> ", "&> ", "> ", ">/"] {
        if command.contains(token_hint) {
            return true;
        }
    }
    if command.contains("<<") {
        return true; // heredoc (covers <<- / <<' / <<" variants).
    }
    let lowered = command.to_ascii_lowercase();
    for kw in [
        " tee ",
        "tee -a",
        "sed -i",
        "sed --in-place",
        "gawk -i",
        "perl -pi",
        "perl -i",
        "ruby -pi",
        "dd of=",
        " of=",
        " install ",
        "patch <",
        "git apply",
        "unzip -o",
        "tar -x",
    ] {
        if lowered.contains(kw) {
            return true;
        }
    }
    // Mutating coreutils with a non-flag argument (flags skipped so
    // `truncate -s 0 f` matches, while `grep -rn foo` style stays clean —
    // the keyword list itself disambiguates).
    for kw in ["cp ", "mv ", "rm ", "mkdir ", "touch ", "truncate ", "ln "] {
        if let Some((_, rest)) = lowered.split_once(kw) {
            let has_operand = rest.split_whitespace().any(|w| !w.starts_with('-'));
            if has_operand {
                return true;
            }
        }
    }
    // Scripted writes via `python -c` / `node -e`.
    if (lowered.contains("python -c")
        || lowered.contains("python3 -c")
        || lowered.contains("node -e"))
        && ((lowered.contains("open(") && lowered.contains("\"w"))
            || lowered.contains("fs.write")
            || lowered.contains("writefilesync"))
    {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// #28a — file-change receipt: a `git status`-level dirty-diff taken before
// and after the command, appended ONCE to THIS tool result's tail.
// ---------------------------------------------------------------------------

/// #28a — one entry of the working-tree dirty set (`git status --porcelain`
/// line, path made repo-relative). No timestamps, no timings, no absolute
/// paths — the receipt must stay noise-free for the model and for prompt-
/// cache stability (it lives only in the tool result, never in the system
/// prompt and never rewriting history).
#[derive(Debug, Clone)]
struct ChangeReceipt {
    files_changed: usize,
    listed: Vec<String>,
    truncated: bool,
}

impl ChangeReceipt {
    fn render(&self) -> String {
        let mut out = format!("\nfiles_changed: {}", self.files_changed);
        for path in &self.listed {
            out.push_str("\n  ");
            out.push_str(path);
        }
        if self.truncated {
            out.push_str(&format!(
                "\n  (+{} more; not listed)",
                self.files_changed - self.listed.len()
            ));
        }
        out
    }
}

/// #28a — snapshot the repo-relative dirty paths (modified + untracked,
/// i.e. `git status --porcelain` lines) at `git status` cost (ms-scale, no
/// index refresh beyond what status itself does; no new dependencies).
/// `None` when the dir is not a git work tree (receipt silently omitted —
/// acceptance ④) or git is unavailable (fail-open).
pub(crate) fn snapshot_dirty_paths(cwd: &Path) -> Option<Vec<String>> {
    // The agent can write this repo's `.git/config`; never let it choose
    // what the kernel's own `git status` runs (fsmonitor, filters, hooks).
    let out = octos_core::agent_repo_git::agent_repo_git(cwd)
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // not a git work tree (or git absent) — omit silently.
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Keep the RAW porcelain line ("XY PATH") so the diff can compare status
    // flips; strip only a rename's "ORIG -> " to keep the destination side.
    Some(
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let path = line.get(3..).unwrap_or("").trim();
                match path.split_once(" -> ") {
                    Some((_, dst)) => format!("{} {}", &line[..2], dst),
                    None => line.to_owned(),
                }
            })
            .collect(),
    )
}

pub(crate) const CHANGE_RECEIPT_MAX_LISTED: usize = 20;

pub(crate) fn diff_to_receipt(
    before: Option<Vec<String>>,
    after: Option<Vec<String>>,
) -> Option<String> {
    // Either side missing the snapshot ⇒ non-git or fail-open: no receipt.
    let (before, after) = (before?, after?);
    let before_set: std::collections::HashSet<&String> = before.iter().collect();
    // CHANGED = after-line not present verbatim in before (new file OR a
    // status flip on the same path — both are real tree changes).
    let mut changed: Vec<String> = after
        .iter()
        .filter(|line| !before_set.contains(*line))
        .map(|line| line.get(3..).unwrap_or(line.as_str()).trim().to_owned())
        .collect();
    if changed.is_empty() {
        return Some("\nfiles_changed: 0".to_owned());
    }
    let total = changed.len();
    changed.sort();
    changed.dedup();
    let truncated = total > CHANGE_RECEIPT_MAX_LISTED;
    let listed: Vec<String> = changed
        .into_iter()
        .take(CHANGE_RECEIPT_MAX_LISTED)
        .collect();
    Some(
        ChangeReceipt {
            files_changed: total,
            listed,
            truncated,
        }
        .render(),
    )
}

#[cfg(all(test, unix))]
mod build_cache_usage_tests {
    use super::*;
    use crate::tools::BuildCacheUsage;
    use std::sync::atomic::{AtomicBool, Ordering};

    async fn child_holds_claim(background: bool, cancel: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::zero();
        let usage = BuildCacheUsage::default();
        ctx.build_cache_usage = Some(usage.clone());
        let cwd = tmp.path().to_path_buf();
        let task = tokio::spawn(async move {
            ShellTool::new(&cwd)
                .execute_with_context(
                    &ctx,
                    &serde_json::json!({
                        "command": "printf started > started; sleep 1; printf done > done",
                        "background": background
                    }),
                )
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !tmp.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if cancel {
            task.abort();
        }
        let released = Arc::new(AtomicBool::new(false));
        let flag = released.clone();
        usage.close_and_when_idle(move || flag.store(true, Ordering::SeqCst));
        assert!(
            !released.load(Ordering::SeqCst),
            "running child must retain claim"
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while !released.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(tmp.path().join("done").exists());
        if !cancel {
            assert!(task.await.unwrap().success);
        }
    }
    #[tokio::test]
    async fn cancelled_foreground_retains_build_cache_claim_until_child_exit() {
        child_holds_claim(false, true).await;
    }
}
