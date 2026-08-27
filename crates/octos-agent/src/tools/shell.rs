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

        let mut cmd = self.sandbox.wrap_command(&command, effective_cwd);
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
        "Execute a shell command and return the output. Use this to run tests, build code, or interact with the filesystem."
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
        let mut cmd = self.sandbox.wrap_command(&input.command, effective_cwd);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        apply_frontend_tool_env(&mut cmd, effective_cwd);
        apply_quarto_tool_env(&mut cmd, &input.command, effective_cwd);
        apply_git_tool_env(&mut cmd, &input.command);
        sanitize_command_env(&mut cmd, &EnvAllowlist::empty());
        apply_harness_event_sink_env(&mut cmd, ctx);

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

        let result = timeout(timeout_duration, child.wait_with_output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code().unwrap_or(-1);

                let mut result_text = String::new();

                if !stdout.is_empty() {
                    result_text.push_str(&stdout);
                }

                if !stderr.is_empty() {
                    if !result_text.is_empty() {
                        result_text.push_str("\n--- stderr ---\n");
                    }
                    result_text.push_str(&stderr);
                }

                if result_text.is_empty() {
                    result_text = "(no output)".to_string();
                }

                // Truncate if too long (reserve space for exit code suffix)
                let exit_suffix = format!("\n\nExit code: {exit_code}");
                const MAX_OUTPUT: usize = 50000;
                octos_core::truncate_utf8(
                    &mut result_text,
                    MAX_OUTPUT - exit_suffix.len(),
                    "\n... (output truncated)",
                );

                result_text.push_str(&exit_suffix);

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
                    success: output.status.success(),
                    ..Default::default()
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                output: format!("Failed to execute command: {e}"),
                success: false,
                ..Default::default()
            }),
            Err(_) => {
                // Graceful shutdown: SIGTERM first, then SIGKILL after grace period.
                // wait_with_output() consumed the Child, so we kill via PID.
                // Use negative PID to target the entire process group.
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
                    let _ = StdCommand::new("taskkill")
                        .args(["/F", "/T", "/PID", &pid.to_string()])
                        .status();
                }
                Ok(ToolResult {
                    output: format!(
                        "Command timed out after {} seconds",
                        timeout_duration.as_secs()
                    ),
                    success: false,
                    ..Default::default()
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runaway guard for the "poll until the background task reaches state X"
    /// loops below. NOT a latency assertion: each loop breaks the instant the
    /// supervisor reports a terminal status, so a generous ceiling costs a passing
    /// run nothing, and a genuinely broken run still fails — just later.
    /// Mirrors `spawn_tests::BACKGROUND_DEADLINE`.
    const BACKGROUND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

    // -----------------------------------------------------------------------
    // #28b — bash_file_writes knob: three positions, escape hatch, and the
    // zero-difference-under-default guarantee.
    // -----------------------------------------------------------------------

    #[test]
    fn bash_file_writes_default_is_allow_and_serializes_round_trip() {
        // Config round-trip: absent key deserializes to `allow` (zero-diff
        // default for existing profiles), and all three values survive.
        #[derive(serde::Deserialize)]
        struct Cfg {
            #[serde(default)]
            bash_file_writes: crate::tools::policy::BashFileWrites,
        }
        let absent: Cfg = serde_json::from_str("{}").expect("absent key");
        assert_eq!(
            absent.bash_file_writes,
            crate::tools::policy::BashFileWrites::Allow
        );
        for (raw, want) in [
            ("allow", crate::tools::policy::BashFileWrites::Allow),
            ("warn", crate::tools::policy::BashFileWrites::Warn),
            ("deny", crate::tools::policy::BashFileWrites::Deny),
        ] {
            let got: Cfg =
                serde_json::from_str(&format!("{{\"bash_file_writes\": \"{raw}\"}}")).expect(raw);
            assert_eq!(got.bash_file_writes, want, "value {raw}");
        }
    }

    #[test]
    fn bash_file_writes_heuristic_matches_write_shapes_only() {
        for cmd in [
            "echo hi > /tmp/f",
            "echo hi >> /tmi/f",
            "cmd 2> /tmp/f",
            "cmd &> /tmp/f",
            "cat <<EOF > f\nhi\nEOF",
            "printf x | tee /tmp/f",
            "sed -i s/a/b/ file",
            "cp a b",
            "mv a b",
            "rm file",
            "mkdir d",
            "touch f",
            "truncate -s 0 f",
            "ln -s a b",
            "python3 -c 'open(\"f\",\"w\")'",
            "node -e 'fs.writeFileSync(\"f\",1)'",
            "dd if=/dev/zero of=f",
            "git apply p.patch",
        ] {
            assert!(command_looks_like_file_write(cmd), "should match: {cmd}");
        }
        for cmd in [
            "ls -la",
            "cat /etc/hosts",
            "grep -rn foo crates/",
            "git status",
            "git diff --stat",
            "cargo build -p octos-agent",
            "cargo test -p octos-agent --lib",
            "echo hello",
            "rg --files | head",
            "sed -n 1,3p file",
            "python3 --version",
            "ps aux | grep octos",
        ] {
            assert!(
                !command_looks_like_file_write(cmd),
                "should NOT match: {cmd}"
            );
        }
    }

    #[test]
    fn bash_file_writes_escape_hatch_allows_explicit_write() {
        assert!(command_allows_write_explicitly(
            "sed -i s/a/b/ file # octos:allow-write"
        ));
        assert!(command_allows_write_explicitly(
            "echo x > /tmp/f # octos:allow-write"
        ));
        // Only the LAST line's comment counts as the hatch.
        assert!(!command_allows_write_explicitly(
            "# octos:allow-write\nsed -i s/a/b/ file"
        ));
        assert!(!command_allows_write_explicitly("sed -i s/a/b/ file"));
    }

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

    #[test]
    fn bash_file_writes_deny_lets_readonly_command_run() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = super::ShellTool::new(temp.path())
            .with_bash_file_writes(crate::tools::policy::BashFileWrites::Deny);
        let out = tokio::runtime::Runtime::new()
            .expect("rt")
            .block_on(tool.execute(&serde_json::json!({ "command": "echo hello" })))
            .expect("execute");
        assert!(out.success);
        assert!(out.output.contains("hello"));
        assert!(!out.output.contains("bash_file_writes"));
    }

    #[test]
    fn bash_file_writes_escape_hatch_runs_under_deny() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("hatch.txt");
        let tool = super::ShellTool::new(temp.path())
            .with_bash_file_writes(crate::tools::policy::BashFileWrites::Deny);
        let cmd = format!("echo hatch > {:?} # octos:allow-write", target);
        let out = tokio::runtime::Runtime::new()
            .expect("rt")
            .block_on(tool.execute(&serde_json::json!({ "command": cmd })))
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(target.exists(), "escape hatch must run the write");
        std::fs::remove_file(&target).ok();
    }

    #[test]
    fn bash_file_writes_allow_is_zero_difference() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = super::ShellTool::new(temp.path());
        let out = tokio::runtime::Runtime::new()
            .expect("rt")
            .block_on(tool.execute(&serde_json::json!({
                "command": "echo zero-diff",
            })))
            .expect("execute");
        assert!(out.success);
        assert!(out.output.contains("zero-diff"));
        // No warn nudge, no deny text, no policy mention at all.
        assert!(!out.output.contains("bash_file_writes"));
        assert!(!out.output.contains("edit_file / diff_edit"));
    }

    #[test]
    fn bash_file_writes_warn_nudges_only_when_files_changed() {
        // The 28a receipt requires a git repo (fail-open elsewhere), so run
        // inside an initialized temp repo.
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(cwd)
            .status()
            .expect("git init");
        let tool = super::ShellTool::new(cwd)
            .with_bash_file_writes(crate::tools::policy::BashFileWrites::Warn);
        // Read-only: no nudge (receipt may report files_changed: 0).
        let out = tokio::runtime::Runtime::new()
            .expect("rt")
            .block_on(tool.execute(&serde_json::json!({ "command": "echo no-change" })))
            .expect("execute");
        assert!(out.success);
        assert!(
            !out.output.contains("bash_file_writes=warn"),
            "output: {}",
            out.output
        );

        // Write: receipt present (28a) + nudge appended once.
        let target = cwd.join("warned.txt");
        let cmd = format!("echo data > {:?}", target);
        let out = tokio::runtime::Runtime::new()
            .expect("rt")
            .block_on(tool.execute(&serde_json::json!({ "command": cmd })))
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            out.output.contains("files_changed: 1"),
            "receipt should be present: {}",
            out.output
        );
        assert!(
            out.output.contains("bash_file_writes=warn"),
            "nudge missing: {}",
            out.output
        );
        std::fs::remove_file(&target).ok();
    }

    #[test]
    fn shell_tool_is_exclusive() {
        // Shell must serialize relative to peers (M8.8) — a mutating command
        // should never race with a parallel read_file on the same path.
        let tool = ShellTool::new(std::env::temp_dir());
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Exclusive);
    }

    #[tokio::test]
    async fn test_timeout_clamped_to_max() {
        let tool = ShellTool::new(std::env::temp_dir());
        let result = tool
            .execute(&serde_json::json!({
                "command": "echo hello",
                "timeout_secs": 999999
            }))
            .await
            .unwrap();
        // Should complete (clamped to 600s, not hang)
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_timeout_zero_clamped_to_min() {
        let tool = ShellTool::new(std::env::temp_dir());
        // timeout_secs: 0 would be clamped to 1 second
        let result = tool
            .execute(&serde_json::json!({
                "command": "echo fast",
                "timeout_secs": 0
            }))
            .await
            .unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_denied_command() {
        let tool = ShellTool::new(std::env::temp_dir());
        let result = tool
            .execute(&serde_json::json!({"command": "rm -rf /"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("denied"));
    }

    #[tokio::test]
    async fn test_ask_command_denied_without_approval() {
        let tool = ShellTool::new(std::env::temp_dir());
        // sudo triggers Ask, which must be denied (no interactive approval)
        let result = tool
            .execute(&serde_json::json!({"command": "sudo ls"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("requires approval"));
    }

    #[tokio::test]
    async fn approval_policy_never_fails_directly_without_prompt() {
        let tool = ShellTool::new(std::env::temp_dir()).with_approval_policy(ApprovalPolicy::Never);
        let result = tool
            .execute(&serde_json::json!({"command": "sudo printf nope"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("approval_policy is never"));
        assert!(!result.output.contains("without interactive approval"));
    }

    #[test]
    fn quarto_command_redirects_home_into_workspace() {
        // Regression (2026-06-08): `quarto render` died under the sandbox
        // with `unable to open database file:
        // ~/Library/Caches/quarto/sass/sass.kv`. Quarto locates that cache
        // via $HOME and ignores QUARTO_CACHE/XDG_CACHE_HOME on macOS, so we
        // redirect HOME into the (sandbox-writable) workspace for quarto
        // commands. No sandbox isolation is weakened — the cache lands
        // inside cwd, which is already writable.
        let cwd = std::env::temp_dir().join(format!("octos-quarto-env-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();

        let mut cmd = tokio::process::Command::new("sh");
        apply_quarto_tool_env(&mut cmd, "cd sites/foo && quarto render", &cwd);

        let home = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| k.to_str() == Some("HOME"))
            .and_then(|(_, v)| v)
            .map(PathBuf::from)
            .expect("HOME must be set for quarto commands");
        assert!(
            home.starts_with(&cwd),
            "quarto HOME must live inside the workspace, got {home:?}"
        );

        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn quarto_command_with_inline_env_prefix_redirects_home() {
        // Regression (2026-06-08): the model invoked quarto with an inline
        // env-var assignment prefix:
        //   `cd sites/foo && QUARTO_PROJECT_DIR=. quarto render index.qmd --to html`
        // `command_invokes_quarto` only matched a segment that *starts with*
        // `quarto `, so the `QUARTO_PROJECT_DIR=. quarto …` segment was not
        // recognised, HOME was left unredirected, and quarto's sass cache
        // escaped to ~/Library/Caches/quarto/sass/sass.kv — denied by the
        // sandbox, breaking theme/syntax-highlight CSS in the rendered page.
        let cwd = std::env::temp_dir().join(format!("octos-quarto-envpfx-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();

        let mut cmd = tokio::process::Command::new("sh");
        apply_quarto_tool_env(
            &mut cmd,
            "cd sites/foo && QUARTO_PROJECT_DIR=. quarto render index.qmd --to html 2>&1",
            &cwd,
        );

        let home = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| k.to_str() == Some("HOME"))
            .and_then(|(_, v)| v)
            .map(PathBuf::from)
            .expect("HOME must be set for quarto invoked with an env-var prefix");
        assert!(
            home.starts_with(&cwd),
            "quarto HOME must live inside the workspace, got {home:?}"
        );

        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn non_quarto_command_does_not_redirect_home() {
        // Least privilege: HOME is only redirected when quarto is invoked,
        // so other tools keep their normal home.
        let cwd = std::env::temp_dir();
        let mut cmd = tokio::process::Command::new("sh");
        apply_quarto_tool_env(&mut cmd, "echo hi && npm run build", &cwd);

        let has_home = cmd
            .as_std()
            .get_envs()
            .any(|(k, _)| k.to_str() == Some("HOME"));
        assert!(!has_home, "non-quarto commands must not redirect HOME");
    }

    #[tokio::test]
    async fn test_shell_sets_frontend_build_env() {
        let cwd = std::env::temp_dir().join(format!("octos-shell-env-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();

        let tool = ShellTool::new(&cwd);
        // The product injects these env vars unconditionally (see
        // `apply_frontend_tool_env`); this test only needs a shell command
        // that echoes them one per line. `printf`/`$VAR` is POSIX-only, so
        // use a `cmd`-native `echo %VAR%` form on Windows.
        #[cfg(windows)]
        let command = "echo %ASTRO_TELEMETRY_DISABLED%&echo %NPM_CONFIG_CACHE%";
        #[cfg(not(windows))]
        let command = "printf '%s\\n%s\\n' \"$ASTRO_TELEMETRY_DISABLED\" \"$NPM_CONFIG_CACHE\"";
        let result = tool
            .execute(&serde_json::json!({ "command": command }))
            .await
            .unwrap();

        assert!(result.success);
        let mut lines = result.output.lines();
        assert_eq!(lines.next(), Some("1"));
        let cache = lines.next().unwrap_or_default();
        assert!(cache.contains("octos-frontend-tool-cache"));
        assert!(!cache.contains(".octos-tool-cache"));
    }

    #[test]
    fn shell_does_not_expose_configured_api_key_to_env_or_echo() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("tools::shell::tests::child_shell_api_key_not_visible")
            .arg("--exact")
            .arg("--ignored")
            .env("OPENAI_API_KEY", "sk-octos-shell-regression")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "child regression test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    #[ignore]
    async fn child_shell_api_key_not_visible() {
        let tool = ShellTool::new(std::env::temp_dir());
        #[cfg(windows)]
        let command = "if defined OPENAI_API_KEY (echo env=%OPENAI_API_KEY%) else (echo env_missing) & echo echo=%OPENAI_API_KEY%";
        #[cfg(not(windows))]
        let command = "if env | grep -q '^OPENAI_API_KEY='; then printf 'env=%s\\n' \"$OPENAI_API_KEY\"; else printf 'env_missing\\n'; fi; printf 'echo=%s\\n' \"$OPENAI_API_KEY\"";

        let result = tool
            .execute(&serde_json::json!({ "command": command }))
            .await
            .unwrap();

        assert!(result.success, "shell command failed: {}", result.output);
        assert!(!result.output.contains("sk-octos-shell-regression"));
        assert!(result.output.contains("env_missing"), "{}", result.output);
    }

    #[test]
    fn detects_git_invocation_in_compound_shell_command() {
        assert!(contains_git_invocation(
            "cd /tmp/repo && git diff -- notes.txt"
        ));
        assert!(contains_git_invocation("GIT_DIR=.git git status --short"));
        assert!(contains_git_invocation("env GIT_DIR=.git git status"));
        assert!(contains_git_invocation("/usr/bin/git log --oneline"));
        assert!(contains_git_invocation(r#""/usr/bin/git" log --oneline"#));
        assert!(contains_git_invocation("time git status --short"));
        assert!(contains_git_invocation("sudo -E git push"));
        assert!(contains_git_invocation("npx git log --oneline"));
        assert!(contains_git_invocation(
            "printf ref | git hash-object --stdin"
        ));
        assert!(contains_git_invocation(
            r#""C:\Program Files\Git\cmd\git.exe" status"#
        ));
        assert!(!contains_git_invocation("printf 'git diff -- notes.txt'"));
        assert!(!contains_git_invocation("grep git README.md"));
        assert!(!contains_git_invocation("FOO=/usr/bin/git echo ok"));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn applies_git_protection_to_wrapped_git_invocation() {
        let tool = ShellTool::new(std::env::temp_dir());
        let result = tool
            .execute(&serde_json::json!({
                "command": "time git --version >/dev/null 2>&1; printf 'global=%s\\nsystem=%s\\n' \"$GIT_CONFIG_GLOBAL\" \"$GIT_CONFIG_NOSYSTEM\""
            }))
            .await
            .unwrap();

        assert!(result.success, "shell command failed: {}", result.output);
        assert!(
            result.output.contains("global=/dev/null"),
            "{}",
            result.output
        );
        assert!(result.output.contains("system=1"), "{}", result.output);
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

    /// Render a canonicalized path the way a child shell's `cd`/`pwd` echoes
    /// it. On Windows `std::fs::canonicalize` yields a `\\?\` verbatim prefix
    /// that the shell never prints, so strip it via `dunce::simplified`
    /// (a lexical no-op on Unix, so the assertions below are unchanged there).
    fn shell_visible_path(p: &std::path::Path) -> String {
        dunce::simplified(p).to_string_lossy().to_string()
    }

    #[cfg(not(windows))]
    const PWD_COMMAND: &str = "pwd";
    #[cfg(windows)]
    const PWD_COMMAND: &str = "cd";

    #[tokio::test]
    async fn shell_uses_scope_workspace_when_present() {
        // When the host has threaded a `SessionScope` onto `ToolContext`
        // AND the scope's workspace matches the tool's construction-time
        // `cwd` (the production wiring in
        // `octos-cli/src/runtime/session.rs`: both derive from
        // `<data_dir>/users/<id>/workspace`), the child process runs
        // with CWD == `scope.workspace()`. This is the load-bearing
        // case for multi-tenant SPA sessions.
        let workspace = tempfile::tempdir().unwrap();
        let canonical_workspace =
            std::fs::canonicalize(workspace.path()).expect("canonicalise workspace");

        // Both scope and ShellTool are constructed with the same
        // workspace (the production wiring), so the migration takes
        // effect and the child process sees the scope workspace.
        let scope = octos_core::SessionScope::solo(canonical_workspace.clone(), vec![])
            .expect("scope construction");
        let tool = ShellTool::new(&canonical_workspace);
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"command": PWD_COMMAND}))
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);

        assert!(
            result
                .output
                .contains(&shell_visible_path(&canonical_workspace)),
            "expected scope workspace ({}) in shell output, got: {}",
            canonical_workspace.display(),
            result.output
        );
    }

    #[tokio::test]
    async fn shell_respects_hinted_workspace_over_session_scope_default() {
        // Codex P1 regression: when the registry's tools were rebound
        // to a hinted workspace (`workspace_hint` flow,
        // `with_workspace_root` in `runtime/session.rs`) but
        // `SessionScope` was built from the canonical
        // `<data_dir>/users/<id>/workspace` (i.e. NOT the hint), the
        // shell tool must honour the hinted `self.cwd` rather than
        // silently relocating the child process into the default
        // data-dir workspace. Without this guard, coding-agent
        // sessions would run builds/tests in the wrong directory.
        let hinted = tempfile::tempdir().unwrap();
        let default_scope_workspace = tempfile::tempdir().unwrap();
        let canonical_hinted = std::fs::canonicalize(hinted.path()).expect("canonicalise hinted");
        let canonical_default_scope = std::fs::canonicalize(default_scope_workspace.path())
            .expect("canonicalise default scope workspace");
        assert_ne!(canonical_hinted, canonical_default_scope);

        let scope = octos_core::SessionScope::solo(canonical_default_scope.clone(), vec![])
            .expect("scope construction");
        // ShellTool is rebound to the HINTED workspace, while the
        // scope still points at the default — exactly the M11
        // workspace_hint code path.
        let tool = ShellTool::new(&canonical_hinted);
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"command": PWD_COMMAND}))
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);

        // The hinted workspace must win — pre-fix this would have
        // contained the default scope workspace instead.
        assert!(
            result
                .output
                .contains(&shell_visible_path(&canonical_hinted)),
            "expected hinted workspace ({}) in shell output, got: {}",
            canonical_hinted.display(),
            result.output
        );
        assert!(
            !result
                .output
                .contains(&shell_visible_path(&canonical_default_scope)),
            "default scope workspace ({}) leaked into shell output: {}",
            canonical_default_scope.display(),
            result.output
        );
    }

    #[tokio::test]
    async fn shell_falls_back_to_self_cwd_when_no_scope() {
        // No scope on the context — behaviour must match the pre-Phase-2D
        // path (child process runs with CWD == construction-time
        // `self.cwd`). Guards the legacy `octos chat` / test-harness
        // codepath that never plumbs a `SessionScope`.
        let legacy_dir = tempfile::tempdir().unwrap();
        let tool = ShellTool::new(legacy_dir.path());
        let ctx = ToolContext::zero();
        assert!(ctx.session_scope.is_none());

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"command": PWD_COMMAND}))
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);

        let canonical_legacy =
            std::fs::canonicalize(legacy_dir.path()).expect("canonicalise legacy dir");
        // The child prints its CWD as the FIRST line of the tool output (the
        // tool appends `\nExit code: N` after it, so the whole string is not a
        // path). Compare by canonical identity rather than a substring match:
        // on windows-latest the runner's TEMP is an 8.3 short name
        // (`C:\Users\RUNNER~1\...`, because the account `runneradmin` exceeds 8
        // chars) while `canonicalize` resolves it to the long name plus a
        // `\\?\` prefix, so a raw-string match spuriously fails (unreproducible
        // where the account name is already short). `canonicalize` collapses
        // short/long spelling and the verbatim prefix to one form and is an
        // idempotent no-op on Unix.
        let printed_cwd = result.output.lines().next().unwrap_or_default().trim();
        let printed_canonical =
            std::fs::canonicalize(printed_cwd).expect("canonicalise shell-printed cwd");
        assert_eq!(
            printed_canonical,
            canonical_legacy,
            "expected legacy cwd ({}) in shell output, got: {}",
            canonical_legacy.display(),
            result.output
        );
    }

    // -----------------------------------------------------------------------
    // Background shell task tracking: a trailing `&` (or `background: true`)
    // registers a supervisor task so the command surfaces in `/ps`, and a
    // watcher flips it terminal when the detached child exits. Mirrors the
    // spawn tool's `with_task_supervisor` + `register_with_lineage` pattern.
    // -----------------------------------------------------------------------

    #[test]
    fn detects_background_via_trailing_ampersand() {
        assert!(is_background_command("sleep 5 &", None));
        assert!(is_background_command("python fetch.py  &", None));
        assert!(is_background_command("vite preview > log 2>&1 &", None));
        // Not background:
        assert!(!is_background_command("npm run build", None));
        assert!(!is_background_command("a && b", None));
        assert!(!is_background_command("echo 2>&1", None));
        assert!(!is_background_command("foo & echo done", None));
        // Explicit arg wins even without a trailing `&`.
        assert!(is_background_command("sleep 5", Some(true)));
        assert!(!is_background_command("sleep 5", Some(false)));
    }

    #[test]
    fn strips_trailing_ampersand_for_own_child() {
        assert_eq!(strip_trailing_ampersand("sleep 5 &"), "sleep 5");
        assert_eq!(strip_trailing_ampersand("sleep 5&"), "sleep 5");
        assert_eq!(
            strip_trailing_ampersand("cmd > log 2>&1 &"),
            "cmd > log 2>&1"
        );
        // No trailing `&` — unchanged.
        assert_eq!(strip_trailing_ampersand("npm run build"), "npm run build");
        // `&&` operator is left intact (not a background request).
        assert_eq!(strip_trailing_ampersand("a && b"), "a && b");
    }

    #[test]
    fn background_label_is_short_and_prefixed() {
        assert_eq!(background_label("npm  run   build"), "shell: npm run build");
        let long = "echo ".to_string() + &"x".repeat(200);
        let label = background_label(&long);
        assert!(label.starts_with("shell: "));
        assert!(label.chars().count() <= "shell: ".chars().count() + 81);
        assert!(label.ends_with('…'));
    }

    #[tokio::test]
    async fn background_command_registers_and_flips_terminal_with_supervisor() {
        use crate::task_supervisor::{TaskStatus, TaskSupervisor};

        let temp = tempfile::tempdir().unwrap();
        let ledger = temp.path().join("tasks.jsonl");
        let supervisor = Arc::new(TaskSupervisor::new());
        supervisor.enable_persistence(&ledger).unwrap();

        let tool = ShellTool::new(std::env::temp_dir()).with_task_supervisor(
            supervisor.clone(),
            "api:test-session",
            ledger.clone(),
        );

        // Trailing `&` → background. Returns immediately.
        let result = tool
            .execute(&serde_json::json!({"command": "sleep 1 &"}))
            .await
            .unwrap();
        assert!(result.success, "{}", result.output);
        assert!(
            result.output.contains("Started background task"),
            "unexpected output: {}",
            result.output
        );

        // The task is registered as active (running) right away — before the
        // 1s sleep exits — which is exactly what makes it visible in `/ps`.
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        assert_eq!(
            tasks.len(),
            1,
            "expected one registered task, got {tasks:?}"
        );
        assert!(
            tasks[0].status.is_active(),
            "task should be active immediately, got {:?}",
            tasks[0].status
        );
        assert!(
            tasks[0].tool_name.contains("sleep 1"),
            "label should reflect the command, got {:?}",
            tasks[0].tool_name
        );

        // Poll until the watcher flips it terminal after the child exits.
        let started = std::time::Instant::now();
        loop {
            let tasks = supervisor.get_tasks_for_session("api:test-session");
            if let Some(t) = tasks.first() {
                if t.status == TaskStatus::Completed {
                    break;
                }
                if t.status == TaskStatus::Failed {
                    panic!("background task failed: {:?}", t.error);
                }
            }
            if started.elapsed() > BACKGROUND_DEADLINE {
                let statuses: Vec<_> = supervisor
                    .get_tasks_for_session("api:test-session")
                    .iter()
                    .map(|t| t.status.as_str().to_string())
                    .collect();
                panic!("background task did not complete in 15s: {statuses:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn explicit_background_arg_registers_task() {
        use crate::task_supervisor::TaskSupervisor;

        let temp = tempfile::tempdir().unwrap();
        let ledger = temp.path().join("tasks.jsonl");
        let supervisor = Arc::new(TaskSupervisor::new());
        supervisor.enable_persistence(&ledger).unwrap();

        let tool = ShellTool::new(std::env::temp_dir()).with_task_supervisor(
            supervisor.clone(),
            "api:bg-arg",
            ledger.clone(),
        );

        // No trailing `&`; the explicit `background: true` arg drives it.
        let result = tool
            .execute(&serde_json::json!({"command": "sleep 1", "background": true}))
            .await
            .unwrap();
        assert!(result.success, "{}", result.output);
        assert!(
            result.output.contains("Started background task"),
            "{}",
            result.output
        );
        assert_eq!(
            supervisor.get_tasks_for_session("api:bg-arg").len(),
            1,
            "explicit background arg must register a task"
        );
    }

    #[tokio::test]
    async fn background_disabled_refuses_explicit_and_trailing_ampersand() {
        // A worker that forbids background execution must refuse BOTH an
        // explicit `background: true` arg and a trailing `&`, returning a
        // failed result WITHOUT spawning a detached child (a `sh -c "cmd &"`
        // must not fall through to a foreground run that self-detaches).
        let tool = ShellTool::new(std::env::temp_dir()).with_background_allowed(false);

        let explicit = tool
            .execute(&serde_json::json!({"command": "sleep 30", "background": true}))
            .await
            .unwrap();
        assert!(!explicit.success, "explicit background must be refused");
        assert!(
            explicit.output.contains("disabled"),
            "unexpected output: {}",
            explicit.output
        );

        let ampersand = tool
            .execute(&serde_json::json!({"command": "sleep 30 &"}))
            .await
            .unwrap();
        assert!(!ampersand.success, "trailing-& background must be refused");
        assert!(
            ampersand.output.contains("disabled"),
            "unexpected output: {}",
            ampersand.output
        );

        // The default (background allowed) is unchanged: a foreground command
        // still runs to completion.
        let allowed = ShellTool::new(std::env::temp_dir());
        let fg = allowed
            .execute(&serde_json::json!({"command": "echo ok"}))
            .await
            .unwrap();
        assert!(
            fg.success,
            "foreground command must still run: {}",
            fg.output
        );
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
    async fn background_command_failure_flips_task_failed() {
        use crate::task_supervisor::{TaskStatus, TaskSupervisor};

        let temp = tempfile::tempdir().unwrap();
        let ledger = temp.path().join("tasks.jsonl");
        let supervisor = Arc::new(TaskSupervisor::new());
        supervisor.enable_persistence(&ledger).unwrap();

        let tool = ShellTool::new(std::env::temp_dir()).with_task_supervisor(
            supervisor.clone(),
            "api:bg-fail",
            ledger.clone(),
        );

        // A non-zero exit must flip the tracked task to Failed.
        let result = tool
            .execute(&serde_json::json!({"command": "false", "background": true}))
            .await
            .unwrap();
        assert!(result.success, "start should succeed: {}", result.output);

        let started = std::time::Instant::now();
        loop {
            let tasks = supervisor.get_tasks_for_session("api:bg-fail");
            if let Some(t) = tasks.first() {
                if t.status == TaskStatus::Failed {
                    assert!(
                        t.error.as_deref().unwrap_or_default().contains("status"),
                        "failure error should mention exit status, got {:?}",
                        t.error
                    );
                    break;
                }
                if t.status == TaskStatus::Completed {
                    panic!("expected failure, task completed");
                }
            }
            if started.elapsed() > BACKGROUND_DEADLINE {
                panic!("background failure not observed in 15s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn background_command_without_supervisor_runs_without_panic() {
        // No supervisor wired (and ToolContext::zero() carries none): the
        // background command must still run detached and return gracefully,
        // reporting it is untracked rather than panicking on a missing handle.
        let tool = ShellTool::new(std::env::temp_dir());
        let result = tool
            .execute(&serde_json::json!({"command": "true &"}))
            .await
            .unwrap();
        assert!(result.success, "{}", result.output);
        assert!(
            result.output.contains("untracked"),
            "expected untracked note, got: {}",
            result.output
        );
        // Give the detached reaper a moment; no panic expected.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn background_command_reads_supervisor_from_tool_context() {
        // Production wiring: the foreground executor threads the SSOT
        // supervisor + session key onto ToolContext (execution.rs). A shell
        // tool with NO explicit builder handle must still register the task by
        // reading `ctx.task_supervisor` / `ctx.parent_session_key`.
        use crate::task_supervisor::TaskSupervisor;

        let supervisor = Arc::new(TaskSupervisor::new());
        let temp = tempfile::tempdir().unwrap();
        supervisor
            .enable_persistence(temp.path().join("tasks.jsonl"))
            .unwrap();

        let tool = ShellTool::new(std::env::temp_dir());
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "shell-ctx-bg".to_string();
        ctx.task_supervisor = Some(supervisor.clone());
        ctx.parent_session_key = Some("api:ctx-session".to_string());

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"command": "sleep 1 &"}))
            .await
            .unwrap();
        assert!(result.success, "{}", result.output);
        assert_eq!(
            supervisor.get_tasks_for_session("api:ctx-session").len(),
            1,
            "task must be registered via ToolContext supervisor"
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
fn command_allows_write_explicitly(command: &str) -> bool {
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
fn command_looks_like_file_write(command: &str) -> bool {
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
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain", "--untracked-files=all"])
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

#[cfg(test)]
mod change_receipt_tests {
    use super::*;

    fn init_repo(dir: &std::path::Path) {
        for args in [
            vec!["init"],
            vec!["config", "user.name", "t"],
            vec!["config", "user.email", "t@t"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(&args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        // seed + commit so status has a baseline tree.
        std::fs::write(dir.join("README.md"), "seed\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["commit", "-m", "init"])
                .status()
                .unwrap()
                .success()
        );
    }

    /// ① real edit → receipt lists exactly that file (repo-relative).
    #[test]
    fn real_edit_lists_the_file() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let before = snapshot_dirty_paths(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn x() {}\n").unwrap();
        let after = snapshot_dirty_paths(dir.path()).unwrap();
        let receipt = diff_to_receipt(Some(before), Some(after)).unwrap();
        assert!(receipt.contains("files_changed: 1"), "receipt: {receipt}");
        assert!(
            receipt.contains("src/lib.rs"),
            "lists the relative path: {receipt}"
        );
        // noise-free: no timestamps/durations/absolute tmp paths.
        assert!(!receipt.contains(&dir.path().display().to_string()));
    }

    /// ② phantom edit (zero-match replace) → files_changed: 0.
    #[test]
    fn phantom_edit_reports_zero() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let before = snapshot_dirty_paths(dir.path()).unwrap();
        // zero-match replace — touches nothing (no file is read or written).
        let after = snapshot_dirty_paths(dir.path()).unwrap();
        let receipt = diff_to_receipt(Some(before), Some(after)).unwrap();
        assert_eq!(receipt.trim(), "files_changed: 0");
    }

    /// ③ a `target/` build artifact never appears — .gitignore filters it.
    #[test]
    fn gitignored_paths_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join(".gitignore"), "/target\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["add", ".gitignore"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["commit", "-m", "ignore"])
                .status()
                .unwrap()
                .success()
        );
        let before = snapshot_dirty_paths(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/artifact.bin"), b"x").unwrap();
        let after = snapshot_dirty_paths(dir.path()).unwrap();
        let receipt = diff_to_receipt(Some(before), Some(after)).unwrap();
        assert_eq!(
            receipt.trim(),
            "files_changed: 0",
            "target/ excluded: {receipt}"
        );
    }

    /// ④ non-git dir → no receipt, no error (snapshot None ⇒ omitted).
    #[test]
    fn non_git_dir_omits_receipt() {
        let dir = tempfile::tempdir().unwrap();
        assert!(snapshot_dirty_paths(dir.path()).is_none());
        assert!(diff_to_receipt(None, None).is_none());
    }

    /// ⑤ cap: >20 changed files list 20 + "(+N more; not listed)".
    #[test]
    fn listing_caps_at_twenty_with_total() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let before = snapshot_dirty_paths(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("many")).unwrap();
        for i in 0..25 {
            std::fs::write(dir.path().join(format!("many/f{i}.txt")), "x\n").unwrap();
        }
        let after = snapshot_dirty_paths(dir.path()).unwrap();
        let receipt = diff_to_receipt(Some(before), Some(after)).unwrap();
        assert!(
            receipt.contains("files_changed: 25"),
            "total counted: {receipt}"
        );
        assert!(
            receipt.contains("more; not listed"),
            "truncation marker: {receipt}"
        );
        let listed_count = receipt
            .lines()
            .filter(|l| l.trim_start().starts_with("many/"))
            .count();
        assert_eq!(listed_count, 20, "exactly 20 listed (cap): {receipt}");
    }
}
