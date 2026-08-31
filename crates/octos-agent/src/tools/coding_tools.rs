//! Codex-compatible P0 coding tool shims.
//!
//! These tools expose the canonical Codex tool names to the model-visible
//! registry. Where Octos already has a native primitive, the implementation
//! delegates to that runtime shape. Where Codex expects an interactive host
//! primitive that Octos does not yet own as an agent tool, the shim returns a
//! typed, non-mutating result instead of silently pretending work happened.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::{
    ConcurrencyClass, TOOL_APPROVAL_CTX, TOOL_CTX, Tool, ToolApprovalDecision, ToolApprovalRequest,
    ToolContext, ToolResult,
};
use crate::policy::{ApprovalPolicy, CommandPolicy, Decision, FileAccessMode, FilesystemScope};
use crate::sandbox::Sandbox;
use crate::subprocess_env::{EnvAllowlist, sanitize_command_env};
use crate::task_supervisor::{RelaunchOpts, TaskRelaunchError, TaskStatus};
use crate::tools::policy::BashFileWrites;

const MAX_EXEC_TIMEOUT_SECS: u64 = 600;
const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 120;
const DEFAULT_EXEC_YIELD_MS: u64 = 1_000;
const MAX_CAPTURE_BYTES: usize = 50_000;

static EXEC_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
static EXEC_SESSIONS: std::sync::OnceLock<Arc<Mutex<HashMap<String, ExecSession>>>> =
    std::sync::OnceLock::new();

fn exec_sessions() -> Arc<Mutex<HashMap<String, ExecSession>>> {
    EXEC_SESSIONS
        .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

#[derive(Clone)]
struct ExecSession {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    output: Arc<Mutex<String>>,
    exit_code: Arc<Mutex<Option<i32>>>,
    sandboxed: bool,
}

fn next_exec_session_id() -> String {
    format!(
        "exec-{}",
        EXEC_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn truncate_output(mut output: String, max_bytes: usize) -> String {
    let cap = max_bytes.max(256);
    octos_core::truncate_utf8(&mut output, cap, "\n... (output truncated)");
    output
}

/// #28c-r1 — resolve the RECEIPT SNAPSHOT ROOT from the command text.
///
/// Coding sessions habitually run `cd <target> && <mutate>` with the tool
/// workdir left at the session workspace root; a receipt snapshotted at the
/// root then reports `files_changed: 0` for a real write inside the target
/// — a FALSE phantom signal, worse than no receipt (outer-loop live
/// verdict, w4). Ruling implementation:
///   * a SINGLE leading `cd <literal-path> && ...` prefix (one token, no
///     `$`/backtick/whitespace, optional matching quotes, `~` expanded)
///     makes that path the snapshot root (`scope: cd-target`);
///   * anything ambiguous — no cd prefix, cd without `&&`, `;` chains,
///     variable paths — falls back to the session workdir
///     (`scope: workdir`), matching the pre-r1 behavior.
///
/// #34g-D — the platform home directory (`HOME` on Unix, `USERPROFILE`
/// fallback on Windows where `HOME` is typically unset). The 34g ruling:
/// the resolver's tilde expansion must not be Unix-only in a resolver that
/// otherwise has no POSIX dependency.
fn platform_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

fn receipt_scope_root(workdir: &Path, command: &str) -> (PathBuf, &'static str) {
    let fallback = || (workdir.to_path_buf(), "workdir");
    let trimmed = command.trim_start();
    let Some(rest) = trimmed.strip_prefix("cd ") else {
        return fallback();
    };
    let Some((target, _tail)) = rest.split_once("&&") else {
        return fallback();
    };
    let target = target.trim();
    if target.is_empty() || target.contains(';') {
        return fallback();
    }
    let literal = if target.len() >= 2 {
        let bytes = target.as_bytes();
        let (first, last) = (bytes[0], bytes[target.len() - 1]);
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            &target[1..target.len() - 1]
        } else {
            target
        }
    } else {
        target
    };
    if literal.is_empty()
        || literal.contains('$')
        || literal.contains('`')
        || literal.contains(' ')
        || literal.contains('\\')
        || literal.starts_with('-')
    {
        return fallback();
    }
    let path = if literal == "~" {
        match platform_home() {
            Some(home) => home,
            None => return fallback(),
        }
    } else if let Some(sub) = literal.strip_prefix("~/") {
        match platform_home() {
            Some(home) => Path::new(&home).join(sub),
            None => return fallback(),
        }
    } else {
        Path::new(literal).to_path_buf()
    };
    let root = if path.is_absolute() {
        path
    } else {
        workdir.join(path)
    };
    (root, "cd-target")
}

use crate::sandbox::sandbox_denial_hint;

/// Session-output payload shared by `exec_command`'s yielded path and
/// `write_stdin` (#2136 review, P1: the principal ASYNC execution path
/// returned permission failures without the [sandbox] explanation the
/// synchronous paths carry). Scans the FULL captured text, truncates,
/// then appends the hint so it survives the cap — same ordering contract
/// as the synchronous assemblers.
fn session_output_payload(
    captured: String,
    exit_code: Option<i32>,
    sandboxed: bool,
    cap: usize,
) -> String {
    let failed = matches!(exit_code, Some(code) if code != 0);
    let hint = sandbox_denial_hint(sandboxed, !failed, &captured);
    let mut output = truncate_output(captured, cap);
    if let Some(hint) = hint {
        output.push_str(hint);
    }
    output
}

fn resolve_optional_workdir(
    base_dir: &Path,
    workdir: Option<&str>,
    filesystem_scope: FilesystemScope,
) -> Result<PathBuf, String> {
    let Some(workdir) = workdir.filter(|value| !value.trim().is_empty()) else {
        return Ok(base_dir.to_path_buf());
    };
    super::resolve_path_with_scope(base_dir, workdir, filesystem_scope)
        .map_err(|_| format!("workdir outside allowed filesystem scope: {workdir}"))
}

async fn append_reader_output<R>(mut reader: R, output: Arc<Mutex<String>>, label: &'static str)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let chunk = String::from_utf8_lossy(&buf[..read]);
        let mut guard = output.lock().await;
        if label == "stderr" && !chunk.is_empty() {
            guard.push_str("\n--- stderr ---\n");
        }
        guard.push_str(&chunk);
        truncate_capture_in_place(&mut guard);
    }
}

/// Trim an accumulating capture buffer to its last [`MAX_CAPTURE_BYTES`] once it
/// grows past twice that, keeping the tail.
///
/// `len - MAX_CAPTURE_BYTES` is a raw BYTE offset that need not fall on a UTF-8
/// char boundary — slicing there directly panics ("byte index N is not a char
/// boundary") when a multibyte char (CJK/emoji) straddles it. Because this runs
/// inside a `tokio::spawn`ed reader task, that panic silently kills the reader
/// and the stream stops capturing. Advance the cut to the next char boundary
/// first so the slice is always valid.
fn truncate_capture_in_place(guard: &mut String) {
    if guard.len() > MAX_CAPTURE_BYTES * 2 {
        let mut keep_from = guard.len().saturating_sub(MAX_CAPTURE_BYTES);
        while keep_from < guard.len() && !guard.is_char_boundary(keep_from) {
            keep_from += 1;
        }
        let trimmed = guard[keep_from..].to_string();
        *guard = format!("... (earlier output truncated)\n{trimmed}");
    }
}

async fn request_command_approval(
    tool_name: &str,
    command: &str,
    cwd: &Path,
    policy: &Arc<dyn CommandPolicy>,
    approval_policy: ApprovalPolicy,
) -> Option<ToolResult> {
    match policy.check(command, cwd) {
        Decision::Allow => None,
        Decision::Deny => Some(ToolResult {
            output: format!(
                "Command denied by security policy: {command}\n\nThis command was blocked because it matches a dangerous pattern."
            ),
            success: false,
            structured_metadata: Some(json!({
                "kind": "command_policy_denied",
                "tool_name": tool_name,
                "command": command,
                "cwd": cwd,
                "policy": "safe_policy",
            })),
            ..Default::default()
        }),
        Decision::Ask => {
            if !approval_policy.allows_prompt() {
                return Some(ToolResult {
                    output: format!(
                        "Command requires approval but approval_policy is never: {command}"
                    ),
                    success: false,
                    structured_metadata: Some(json!({
                        "kind": "approval_required",
                        "tool_name": tool_name,
                        "command": command,
                        "cwd": cwd,
                        "approval_policy": "never",
                    })),
                    ..Default::default()
                });
            }
            let Some(requester) = TOOL_APPROVAL_CTX.try_with(Clone::clone).ok() else {
                return Some(ToolResult {
                    output: format!(
                        "Command requires approval and was denied: {command}\n\nNo interactive approval channel is available."
                    ),
                    success: false,
                    structured_metadata: Some(json!({
                        "kind": "approval_unavailable",
                        "tool_name": tool_name,
                        "command": command,
                        "cwd": cwd,
                    })),
                    ..Default::default()
                });
            };
            let tool_id = TOOL_CTX
                .try_with(|ctx| ctx.tool_id.clone())
                .unwrap_or_default();
            let decision = requester
                .request_approval(ToolApprovalRequest {
                    tool_id,
                    tool_name: tool_name.to_owned(),
                    title: "Approve command".to_owned(),
                    body: format!("Run command: {command}"),
                    command: Some(command.to_owned()),
                    cwd: Some(cwd.to_string_lossy().into_owned()),
                })
                .await;
            if matches!(decision, ToolApprovalDecision::Deny) {
                Some(ToolResult {
                    output: format!("Command denied by user approval: {command}"),
                    success: false,
                    structured_metadata: Some(json!({
                        "kind": "approval_denied",
                        "tool_name": tool_name,
                        "command": command,
                        "cwd": cwd,
                    })),
                    ..Default::default()
                })
            } else {
                None
            }
        }
    }
}

// NOTE: `ApplyPatchTool` moved to `super::apply_patch` (#1773) — the full
// Codex-envelope implementation with Move support and two-phase
// validate-then-apply atomicity lives there now.

#[derive(Debug, Deserialize)]
struct ExecCommandInput {
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    tty: Option<bool>,
}

pub struct ExecCommandTool {
    /// #28d — bash file-writes knob (shared judge/escape-hatch; see
    /// `super::shell`), loaded ONCE at construction.
    bash_file_writes: BashFileWrites,
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    policy: Arc<dyn CommandPolicy>,
    approval_policy: ApprovalPolicy,
    sandbox: Arc<dyn Sandbox>,
}

impl ExecCommandTool {
    pub fn new(base_dir: impl Into<PathBuf>, sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            policy: Arc::new(crate::policy::SafePolicy::default()),
            approval_policy: ApprovalPolicy::Ask,
            sandbox,
            bash_file_writes: BashFileWrites::default(),
        }
    }

    /// #28d — set the bash file-writes knob (defaults to `allow`).
    pub fn with_bash_file_writes(mut self, mode: BashFileWrites) -> Self {
        self.bash_file_writes = mode;
        self
    }

    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    pub fn with_policy(mut self, policy: Arc<dyn CommandPolicy>) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_approval_policy(mut self, approval_policy: ApprovalPolicy) -> Self {
        self.approval_policy = approval_policy;
        self
    }
}

#[async_trait]
impl Tool for ExecCommandTool {
    fn name(&self) -> &str {
        "exec_command"
    }

    fn description(&self) -> &str {
        "Run a shell command. For long-running commands, set tty=true or yield_time_ms to receive a session_id and continue with write_stdin."
    }

    fn tags(&self) -> &[&str] {
        &["runtime", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string"},
                "command": {"type": "string"},
                "workdir": {"type": "string"},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": MAX_EXEC_TIMEOUT_SECS},
                "yield_time_ms": {"type": "integer", "minimum": 0},
                "max_output_tokens": {"type": "integer", "minimum": 1},
                "tty": {"type": "boolean"}
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: ExecCommandInput =
            serde_json::from_value(args.clone()).wrap_err("invalid exec_command input")?;
        let Some(command) = input.cmd.clone().or_else(|| input.command.clone()) else {
            return Ok(ToolResult {
                output: "exec_command requires cmd".to_string(),
                success: false,
                ..Default::default()
            });
        };
        let cwd = match resolve_optional_workdir(
            &self.base_dir,
            input.workdir.as_deref(),
            self.filesystem_scope,
        ) {
            Ok(cwd) => cwd,
            Err(output) => {
                return Ok(ToolResult {
                    output,
                    success: false,
                    ..Default::default()
                });
            }
        };
        if let Some(result) = request_command_approval(
            self.name(),
            &command,
            &cwd,
            &self.policy,
            self.approval_policy,
        )
        .await
        {
            return Ok(result);
        }

        if input.tty.unwrap_or(false) || input.yield_time_ms.is_some() {
            self.spawn_session(command, cwd, input).await
        } else {
            self.run_to_completion(command, cwd, input).await
        }
    }
}

impl ExecCommandTool {
    async fn run_to_completion(
        &self,
        command: String,
        cwd: PathBuf,
        input: ExecCommandInput,
    ) -> Result<ToolResult> {
        let timeout_secs = input
            .timeout_secs
            .unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)
            .clamp(1, MAX_EXEC_TIMEOUT_SECS);
        // #28d — deny knob on the CODING bash path: same heuristic judge
        // and escape hatch as ShellTool (single shared implementation in
        // `super::shell`), loaded at construction. A refusal happens BEFORE
        // any spawn/approval/snapshot.
        if self.bash_file_writes == BashFileWrites::Deny
            && !super::shell::command_allows_write_explicitly(&command)
            && super::shell::command_looks_like_file_write(&command)
        {
            return Ok(ToolResult {
                output: "Command refused by tool_policy.bash_file_writes=deny (it looks like a \
                         file-writing shell command). Use the edit_file / diff_edit tools for \
                         code changes instead. If this refusal is a false positive, append the \
                         comment `# octos:allow-write` to the command line to run it \
                         explicitly. Command: "
                    .to_owned()
                    + &command,
                success: false,
                ..Default::default()
            });
        }
        // #28c-r1 — receipt snapshot root: a leading literal `cd X &&`
        // prefix (the coding-session idiom) makes X the root; otherwise the
        // session workdir. Prevents the false `files_changed: 0` on writes
        // outside the workspace root (outer-loop live verdict).
        let (snapshot_root, receipt_scope) = receipt_scope_root(&cwd, &command);
        // #28c — BEFORE snapshot for the file-change receipt, reusing the
        // SAME shared 28a module as ShellTool. `None` on non-git/fail-open
        // omits the receipt.
        // Fail closed on an unhonorable sandbox config BEFORE spawning
        // anything: the typed refusal (with its per-OS remediation) IS the
        // tool result. The wrap-level refusal command would also fail, but a
        // background command discards its stderr, and the model deserves the
        // full remediation text, not a truncated stderr line.
        if let Some(refusal) = self.sandbox.refusal() {
            return Ok(ToolResult {
                output: refusal.to_string(),
                success: false,
                ..Default::default()
            });
        }

        let dirty_before = super::shell::snapshot_dirty_paths(&snapshot_root);
        let mut cmd = self.sandbox.wrap_command(&command, &cwd);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        // Put the child in its own process group so the timeout path can
        // signal the WHOLE tree (wrapper shell + grandchildren) with a
        // negative-PID kill — mirrors the `bash` tool path. Without this the
        // negative-PID kill targets a group the child was never placed in, so
        // a backgrounded `sleep` survives the timeout and can mutate the
        // workspace after the tool has reported failure.
        #[cfg(unix)]
        cmd.process_group(0);
        sanitize_command_env(&mut cmd, &EnvAllowlist::empty());
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Ok(ToolResult {
                    output: format!("Failed to execute command: {error}"),
                    success: false,
                    ..Default::default()
                });
            }
        };
        // Capture the pid BEFORE `wait_with_output` consumes the child — the
        // timeout arm needs it to kill the process tree (dropping the wait
        // future does NOT kill a tokio child).
        let child_pid = child.id();
        // Armed for the whole wait: a dropped future (user interrupt ->
        // `agent_task.abort()`) reaches neither arm below. See `ChildGroupGuard`.
        let mut group_guard = ChildGroupGuard::new(child_pid);
        let result = timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;
        group_guard.disarm();
        match result {
            Ok(Ok(output)) => {
                let mut text = String::new();
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                if !output.stderr.is_empty() {
                    if !text.is_empty() {
                        text.push_str("\n--- stderr ---\n");
                    }
                    text.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                if text.is_empty() {
                    text.push_str("(no output)");
                }
                text.push_str(&format!(
                    "\n\nExit code: {}",
                    output.status.code().unwrap_or(-1)
                ));
                // #28c — file-change receipt on the coding-session exec
                // path too (same shared 28a module; same five acceptance
                // semantics as ShellTool / BashTool).
                if let Some(receipt) = super::shell::diff_to_receipt(
                    dirty_before,
                    super::shell::snapshot_dirty_paths(&snapshot_root),
                ) {
                    text.push_str(&receipt);
                    text.push_str(&format!("\nscope: {receipt_scope}"));
                    // #28d — warn knob: nudge ONLY when files actually
                    // changed (same after-snapshot as the receipt; no
                    // second scan). Zero behavior under `allow`.
                    if self.bash_file_writes == BashFileWrites::Warn
                        && !receipt.trim_end().ends_with("files_changed: 0")
                    {
                        text.push_str(
                            "\nnote: prefer the edit_file / diff_edit tools for code changes (tool_policy.bash_file_writes=warn)",
                        );
                    }
                }
                // Scan BEFORE truncation (the denial line may be what gets
                // cut), append AFTER (so the hint itself survives the cut).
                let hint =
                    sandbox_denial_hint(!self.sandbox.is_noop(), output.status.success(), &text);
                let max = input.max_output_tokens.unwrap_or(MAX_CAPTURE_BYTES);
                let mut out = truncate_output(text, max);
                if let Some(hint) = hint {
                    out.push_str(hint);
                }
                Ok(ToolResult {
                    output: out,
                    success: output.status.success(),
                    ..Default::default()
                })
            }
            Ok(Err(error)) => Ok(ToolResult {
                output: format!("Failed to execute command: {error}"),
                success: false,
                ..Default::default()
            }),
            Err(_) => {
                // Dropping the wait future does NOT kill a tokio child, so
                // the wrapper shell and any grandchildren keep running. Kill
                // the whole process group/tree (SIGTERM → 500ms grace →
                // SIGKILL on Unix, `taskkill /F /T` on Windows) — the same
                // helper the `bash` tool uses.
                kill_timed_out_child(child_pid).await;
                Ok(ToolResult {
                    output: format!("Command timed out after {timeout_secs} seconds"),
                    success: false,
                    ..Default::default()
                })
            }
        }
    }

    async fn spawn_session(
        &self,
        command: String,
        cwd: PathBuf,
        input: ExecCommandInput,
    ) -> Result<ToolResult> {
        // Fail closed on an unhonorable sandbox config BEFORE spawning
        // anything: the typed refusal (with its per-OS remediation) IS the
        // tool result. The wrap-level refusal command would also fail, but a
        // background command discards its stderr, and the model deserves the
        // full remediation text, not a truncated stderr line.
        if let Some(refusal) = self.sandbox.refusal() {
            return Ok(ToolResult {
                output: refusal.to_string(),
                success: false,
                ..Default::default()
            });
        }

        let mut cmd = self.sandbox.wrap_command(&command, &cwd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        sanitize_command_env(&mut cmd, &EnvAllowlist::empty());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Ok(ToolResult {
                    output: format!("Failed to execute command: {error}"),
                    success: false,
                    ..Default::default()
                });
            }
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let session_id = next_exec_session_id();
        let output = Arc::new(Mutex::new(String::new()));
        let exit_code = Arc::new(Mutex::new(None));
        // #2136 review: after the child exits, give the pipe readers a
        // BOUNDED grace to drain before publishing the exit code, then
        // publish regardless. A plain reader-join deadlocked when a
        // descendant (a backgrounded `server &`, a daemon that inherits
        // stdout) keeps a pipe write-end open — EOF never arrives and the
        // session reported `running` forever. The grace closes the
        // round-2 race (a fast-exiting command's final output — e.g. a
        // denial line — is captured within the window, since its fds close
        // at exit) without hanging on a surviving descendant; combined
        // with sampling the exit code BEFORE the output, "not running"
        // implies "output drained" in the common case.
        const READER_DRAIN_GRACE: Duration = Duration::from_millis(200);
        let stdout_reader = stdout
            .map(|stdout| tokio::spawn(append_reader_output(stdout, output.clone(), "stdout")));
        let stderr_reader = stderr
            .map(|stderr| tokio::spawn(append_reader_output(stderr, output.clone(), "stderr")));
        let exit_code_for_wait = exit_code.clone();
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|status| status.code());
            let _ = tokio::time::timeout(READER_DRAIN_GRACE, async {
                if let Some(handle) = stdout_reader {
                    let _ = handle.await;
                }
                if let Some(handle) = stderr_reader {
                    let _ = handle.await;
                }
            })
            .await;
            *exit_code_for_wait.lock().await = Some(code.unwrap_or(-1));
        });
        exec_sessions().lock().await.insert(
            session_id.clone(),
            ExecSession {
                stdin: Arc::new(Mutex::new(stdin)),
                output: output.clone(),
                exit_code: exit_code.clone(),
                sandboxed: !self.sandbox.is_noop(),
            },
        );
        tokio::time::sleep(Duration::from_millis(
            input.yield_time_ms.unwrap_or(DEFAULT_EXEC_YIELD_MS),
        ))
        .await;
        // #2136 review round 3, P2: sample the exit code FIRST, then the
        // output. The exit-code task sets the code only AFTER joining the
        // pipe readers, so `code.is_some()` (not running) guarantees the
        // output buffer is fully drained — reading output after the code
        // therefore never sees a stale/empty capture with running:false.
        let code = *exit_code.lock().await;
        let captured = output.lock().await.clone();
        Ok(ToolResult {
            output: json!({
                "session_id": session_id,
                "running": code.is_none(),
                "exit_code": code,
                "output": session_output_payload(
                    captured,
                    code,
                    !self.sandbox.is_noop(),
                    input.max_output_tokens.unwrap_or(MAX_CAPTURE_BYTES),
                ),
            })
            .to_string(),
            success: true,
            ..Default::default()
        })
    }
}

#[derive(Debug, Deserialize)]
struct WriteStdinInput {
    session_id: String,
    #[serde(default)]
    chars: Option<String>,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

pub struct WriteStdinTool;

#[async_trait]
impl Tool for WriteStdinTool {
    fn name(&self) -> &str {
        "write_stdin"
    }

    fn description(&self) -> &str {
        "Write characters to a running exec_command session and return recent captured output."
    }

    fn tags(&self) -> &[&str] {
        &["runtime", "code"]
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["session_id"],
            "properties": {
                "session_id": {"type": "string"},
                "chars": {"type": "string"},
                "yield_time_ms": {"type": "integer", "minimum": 0},
                "max_output_tokens": {"type": "integer", "minimum": 1}
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: WriteStdinInput =
            serde_json::from_value(args.clone()).wrap_err("invalid write_stdin input")?;
        let Some(session) = exec_sessions().lock().await.get(&input.session_id).cloned() else {
            return Ok(ToolResult {
                output: format!("unknown exec session: {}", input.session_id),
                success: false,
                ..Default::default()
            });
        };
        if let Some(chars) = input.chars.as_deref() {
            let mut stdin = session.stdin.lock().await;
            if let Some(stdin) = stdin.as_mut() {
                stdin.write_all(chars.as_bytes()).await?;
                stdin.flush().await?;
            } else {
                return Ok(ToolResult {
                    output: format!("exec session {} has no open stdin", input.session_id),
                    success: false,
                    ..Default::default()
                });
            }
        }
        tokio::time::sleep(Duration::from_millis(input.yield_time_ms.unwrap_or(250))).await;
        // #2136 review round 3, P2: code before output (see spawn_session).
        let code = *session.exit_code.lock().await;
        let output = session.output.lock().await.clone();
        Ok(ToolResult {
            output: json!({
                "session_id": input.session_id,
                "running": code.is_none(),
                "exit_code": code,
                "output": session_output_payload(
                    output,
                    code,
                    session.sandboxed,
                    input.max_output_tokens.unwrap_or(MAX_CAPTURE_BYTES),
                ),
            })
            .to_string(),
            success: true,
            ..Default::default()
        })
    }
}

macro_rules! simple_codex_tool {
    ($name:ident, $tool_name:literal, $description:literal, $body:expr) => {
        pub struct $name;

        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &str {
                $tool_name
            }

            fn description(&self) -> &str {
                $description
            }

            fn tags(&self) -> &[&str] {
                &["code"]
            }

            fn input_schema(&self) -> Value {
                json!({"type": "object", "additionalProperties": true})
            }

            async fn execute(&self, args: &Value) -> Result<ToolResult> {
                $body(self, args, &ToolContext::zero()).await
            }

            async fn execute_with_context(
                &self,
                ctx: &ToolContext,
                args: &Value,
            ) -> Result<ToolResult> {
                $body(self, args, ctx).await
            }
        }
    };
}

/// Normalize the (permissive, Codex-shaped) `update_plan` arguments into a
/// typed [`UiPlanRecord`]. Accepts item text under `step` / `title` / `content`
/// and tolerates a few status spellings; assigns a stable 1-based `id` when the
/// caller doesn't supply one so downstream clients can re-render in place.
pub(crate) fn normalize_plan(args: &Value, now_ms: i64) -> octos_core::ui_protocol::UiPlanRecord {
    use octos_core::ui_protocol::{PlanItemStatus, UiPlanItem, UiPlanRecord};
    let items = args
        .get("plan")
        .or_else(|| args.get("items"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, item)| {
                    let title = ["step", "title", "content", "text"]
                        .iter()
                        .find_map(|k| item.get(*k).and_then(|v| v.as_str()))
                        .unwrap_or_default()
                        .to_string();
                    let status = match item.get("status").and_then(|v| v.as_str()) {
                        Some("in_progress" | "in-progress" | "active" | "running") => {
                            PlanItemStatus::InProgress
                        }
                        Some("completed" | "complete" | "done") => PlanItemStatus::Completed,
                        _ => PlanItemStatus::Pending,
                    };
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| (i + 1).to_string());
                    let priority = item
                        .get("priority")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    UiPlanItem {
                        id,
                        title,
                        status,
                        priority,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let title = ["explanation", "title"]
        .iter()
        .find_map(|k| args.get(*k).and_then(|v| v.as_str()))
        .map(str::to_string);
    UiPlanRecord {
        items,
        title,
        updated_at_ms: now_ms,
    }
}

async fn update_plan_body(_: &dyn Tool, args: &Value, ctx: &ToolContext) -> Result<ToolResult> {
    // Live path: stream the checklist as a `plan/updated` notification (gated
    // by `plan.todos.v1` on the serve side). `ToolContext::zero()` carries a
    // `SilentReporter`, so the `execute()` entry point is a safe no-op.
    let record = normalize_plan(args, chrono::Utc::now().timestamp_millis());
    if let Ok(plan_value) = serde_json::to_value(&record) {
        ctx.reporter
            .report(crate::progress::ProgressEvent::PlanUpdated { plan: plan_value });
    }
    // Legacy path preserved for Codex/ACP surfaces that read the plan off the
    // `tool/completed` structured_metadata.
    Ok(ToolResult {
        output: json!({"ok": true, "plan": args}).to_string(),
        success: true,
        structured_metadata: Some(json!({"codex_tool": "update_plan", "plan": args})),
        ..Default::default()
    })
}

async fn request_user_input_body(
    _: &dyn Tool,
    args: &Value,
    _: &ToolContext,
) -> Result<ToolResult> {
    Ok(ToolResult {
        output: json!({
            "ok": true,
            "kind": "user_input_request",
            "status": "requested",
            "request": args,
            "response": null,
            "message": "User input request recorded in the transcript; no synchronous host response channel is attached to this runtime (non-interactive or unattended run). Do NOT wait or re-ask: proceed with your best judgment, state the assumption in one line, and continue the task so the user can redirect you later if needed."
        })
        .to_string(),
        success: true,
        structured_metadata: Some(json!({
            "codex_tool": "request_user_input",
            "request": args,
            "host_response_channel": "not_attached",
        })),
        ..Default::default()
    })
}

pub struct SpawnAgentTool {
    delegate: Option<Arc<dyn Tool>>,
}

impl Default for SpawnAgentTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SpawnAgentTool {
    pub fn new() -> Self {
        Self { delegate: None }
    }

    pub fn with_delegate(delegate: Arc<dyn Tool>) -> Self {
        Self {
            delegate: Some(delegate),
        }
    }
}

fn codex_items_text(args: &Value) -> Vec<String> {
    args.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("item");
            let body = item
                .get("text")
                .or_else(|| item.get("name"))
                .or_else(|| item.get("path"))
                .or_else(|| item.get("image_url"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())?;
            Some(format!("[{kind}] {body}"))
        })
        .collect()
}

fn append_instruction(existing: Option<String>, instruction: String) -> Option<String> {
    if instruction.trim().is_empty() {
        return existing;
    }
    Some(match existing {
        Some(existing) if !existing.trim().is_empty() => format!("{existing}\n{instruction}"),
        _ => instruction,
    })
}

/// Normalise the loose Codex `spawn_agent` arg shape into the strict
/// `spawn` tool argument layout, optionally folding a resolved
/// [`crate::role_template::RoleTemplate`] into the spawn payload.
///
/// Issue #971 (M14-C): when a caller passes `role: "reviewer"` (or any
/// other registered template name), the alias resolves the template
/// once at the spawn_agent boundary and forwards the template's
/// `allowed_tools` budget + `prompt_prefix` to the native spawn tool.
/// Inline-wins semantics still apply: a caller-supplied `allowed_tools`
/// array overrides the template's, and a caller-supplied
/// `additional_instructions` is appended TO (not replaced BY) the
/// template's prompt prefix.
fn normalize_spawn_agent_args_with_role(
    args: &Value,
    role: Option<&crate::role_template::RoleTemplate>,
) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(input) = args.as_object() {
        for key in [
            "task",
            "label",
            "mode",
            "allowed_tools",
            "context",
            "model",
            "context_window",
            "additional_instructions",
            "workflow",
            "backend",
            "agent_mcp_tool_name",
            "agent_definition_id",
            "role",
        ] {
            if let Some(value) = input.get(key) {
                out.insert(key.to_string(), value.clone());
            }
        }
    }

    let mut task = out
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            args.get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| args.to_string());
    let item_text = codex_items_text(args);
    if !item_text.is_empty() {
        task.push_str("\n\nItems:\n");
        task.push_str(&item_text.join("\n"));
    }
    out.insert("task".to_string(), Value::String(task));
    out.entry("mode".to_string())
        .or_insert_with(|| Value::String("background".to_string()));

    if !out.contains_key("label") {
        if let Some(agent_type) = args
            .get("agent_type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            out.insert(
                "label".to_string(),
                Value::String(format!("codex-{agent_type}")),
            );
        }
    }
    // PR #1177 codex round-3 P2 fix: a client that serialises an
    // unset `role` as `null` or `""` would otherwise leak that blank
    // value through to the spawn delegate, whose `apply_role_template`
    // treats blank/None as "no role" and skips the prompt-prefix
    // injection — even though `SpawnAgentTool::execute_with_context`
    // resolved a template from `agent_type` and stamped the task with
    // that role. Drop the blank value here so either the resolved
    // template below OR the spawn delegate's `apply_role_template`
    // sees a clean absence; the resolved value is then written back
    // unconditionally.
    let blank_role = match out.get("role") {
        Some(Value::Null) | None => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        _ => false,
    };
    if blank_role {
        out.remove("role");
    }
    // Codex round-3 P2 fix: when an explicit `role` is absent AND a
    // recognised `agent_type` alias is set, write the canonical role
    // name so the spawn delegate's `apply_role_template` resolves the
    // same template the boundary already stamped on the BackgroundTask.
    if !out.contains_key("role") {
        if let Some(role_template) = args
            .get("agent_type")
            .and_then(Value::as_str)
            .and_then(crate::role_template::RoleTemplate::for_codex_agent_type)
        {
            out.insert(
                "role".to_string(),
                Value::String(role_template.name.to_string()),
            );
        }
    }
    // Codex round-3 P2 fix: when the boundary already resolved a
    // template (either from an explicit `role` arg or from the
    // `agent_type` alias path), authoritatively write the canonical
    // template name. This keeps the spawn delegate's view in lock-step
    // with the BackgroundTask.role stamp even when the caller passed
    // e.g. `role: " Reviewer "` (whitespace / wrong case) or relied
    // on the `agent_type` alias.
    if let Some(template) = role {
        out.insert("role".to_string(), Value::String(template.name.to_string()));
    }
    if !out.contains_key("model") {
        if let Some(model) = args.get("model").and_then(Value::as_str) {
            out.insert("model".to_string(), Value::String(model.to_string()));
        }
    }

    let mut extra = out
        .get("additional_instructions")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    // Issue #971 (M14-C): when the caller resolved a role template,
    // seed the spawn payload's `allowed_tools` with the template's
    // spawn-compatible budget. The native `SpawnTool::apply_role_template`
    // is the authoritative layer for role-derived metadata
    // (prompt-prefix prepending happens there so the path is uniform
    // for ALL `role`-bearing spawns, not just `spawn_agent`-aliased
    // ones); but `apply_role_template` falls back to
    // `RoleTemplate::allowed_tools_vec()` which still carries
    // `group:*` identifiers. Those raw group entries would then fail
    // `ensure_subagent_tools_available` (it does exact-name lookup).
    // Pre-populating the inline `allowed_tools` with
    // `to_spawn_compatible_allow()` short-circuits the fallback so the
    // wire payload only carries names the child's `with_builtins`
    // registry actually has.
    //
    // Codex iter-2 P2 (PR #1171 → PR #1177): the prefix prepending
    // is INTENTIONALLY NOT done here — `spawn.rs::apply_role_template`
    // prepends `template.prompt_prefix` to `additional_instructions`
    // for every native-delegate spawn, so doing it again at this
    // boundary would double the prefix in the child's system context.
    if let Some(template) = role {
        // Issue #971 codex P1 fix iteration 2: only treat a NON-EMPTY
        // `allowed_tools` array as an inline override. An empty array
        // is interpreted by the native spawn tool as "all builtins",
        // so without this guard a client that always serialises
        // `allowed_tools: []` would bypass the role's restricted
        // budget and silently receive write/shell/browser tools on a
        // reviewer/explorer/test_worker spawn.
        //
        // Issue #971 codex iter-5 P1 fix: when `agent_definition_id`
        // is set, `SpawnTool::apply_agent_definition` treats any
        // non-empty `allowed_tools` on the inline Input as a CALLER
        // override and skips the manifest's `tools` allow-list. Role
        // defaults are NOT caller overrides — they should defer to
        // the manifest's allow-list. Skip the role injection when a
        // manifest reference is present so the manifest's `tools`
        // gate fires; the role still contributes the prompt prefix
        // through `spawn.rs::apply_role_template`.
        let manifest_id_present = out
            .get("agent_definition_id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let inline_allowed_present = out
            .get("allowed_tools")
            .and_then(Value::as_array)
            .is_some_and(|arr| !arr.is_empty());
        if !inline_allowed_present && !manifest_id_present {
            // Drop any caller-supplied empty array so the role's
            // budget is the one that ships.
            out.remove("allowed_tools");
            // Issue #971 codex P1 fix: filter expanded `group:*`
            // entries to those the child's `with_builtins` registry
            // actually has. The prior wiring forwarded raw expansions
            // including `recall_memory` / `synthesize_research` /
            // `save_memory` / `spawn` — none of which `with_builtins`
            // registers — so every default role-based spawn failed
            // `SpawnTool::ensure_subagent_tools_available` before the
            // child could run. `to_spawn_compatible_allow` returns
            // the intersection of the role's expansion with the
            // builtin set.
            out.insert(
                "allowed_tools".to_string(),
                Value::Array(
                    template
                        .to_spawn_compatible_allow()
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                ),
            );
        } else if manifest_id_present && !inline_allowed_present {
            // Codex iter-5 P1: ensure we don't pass a stale empty
            // array through when a manifest is set. Removing the key
            // entirely lets `apply_agent_definition` install the
            // manifest's `tools` allow-list unmolested.
            out.remove("allowed_tools");
        }
    }
    if let Some(agent_type) = args
        .get("agent_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        extra = append_instruction(extra, format!("Requested Codex agent_type: {agent_type}."));
    }
    if let Some(effort) = args
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        extra = append_instruction(extra, format!("Requested reasoning_effort: {effort}."));
    }
    if args
        .get("fork_context")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        extra = append_instruction(
            extra,
            "Fork current parent context if the runtime has a child context manager bound."
                .to_string(),
        );
    }
    if let Some(extra) = extra {
        out.insert("additional_instructions".to_string(), Value::String(extra));
    }

    Value::Object(out)
}

fn newest_spawned_task(
    supervisor: &crate::task_supervisor::TaskSupervisor,
    before: &HashSet<String>,
) -> Option<crate::task_supervisor::BackgroundTask> {
    supervisor
        .get_all_tasks()
        .into_iter()
        .filter(|task| !before.contains(&task.id))
        .max_by_key(|task| task.started_at)
}

async fn spawn_agent_without_delegate(args: &Value, ctx: &ToolContext) -> Result<ToolResult> {
    if let Some(supervisor) = ctx.task_supervisor.as_ref() {
        let task_id = supervisor.register_with_input(
            "spawn_agent",
            &format!("codex-spawn-{}", next_exec_session_id()),
            ctx.parent_session_key.as_deref(),
            Some(args.clone()),
        );
        supervisor.mark_failed(
            &task_id,
            "spawn_agent requires the session runtime to register a native spawn tool delegate"
                .to_string(),
        );
        return Ok(ToolResult {
            output: json!({
                "agent_id": task_id,
                "status": "failed",
                "message": "No native Octos spawn tool is bound behind spawn_agent in this ToolRegistry."
            })
            .to_string(),
            success: false,
            ..Default::default()
        });
    }
    Ok(ToolResult {
        output: "spawn_agent requires a task supervisor and native spawn delegate".to_string(),
        success: false,
        ..Default::default()
    })
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "Start a Codex-compatible subagent. When Octos' native spawn tool is registered, this forwards to it and returns the supervised agent handle."
    }

    fn tags(&self) -> &[&str] {
        &["gateway", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> Value {
        // Issue #971 (M14-C): advertise `role` and enumerate the four
        // backend-owned templates so the LLM can discover them without
        // probing the server. Enum values stay in sync with
        // `RoleTemplate::all()` via the static role constants.
        let role_names: Vec<Value> = crate::role_template::RoleTemplate::all()
            .iter()
            .map(|tpl| Value::String(tpl.name.to_string()))
            .collect();
        json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"},
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "type": {"type": "string"},
                            "text": {"type": "string"},
                            "name": {"type": "string"},
                            "path": {"type": "string"},
                            "image_url": {"type": "string"}
                        }
                    }
                },
                "agent_type": {"type": "string"},
                "fork_context": {"type": "boolean"},
                "model": {"type": "string"},
                "reasoning_effort": {"type": "string"},
                "task": {"type": "string"},
                "label": {"type": "string"},
                "role": {"type": "string"},
                "mode": {"type": "string", "enum": ["background", "sync"]},
                "allowed_tools": {"type": "array", "items": {"type": "string"}},
                "role": {
                    "type": "string",
                    "description": "Optional backend-owned role template that resolves to a tool budget + sandbox + model preference + prompt prefix. When set, the resolved template seeds the spawn payload; inline `allowed_tools` still wins.",
                    "enum": role_names,
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        // Issue #971 (M14-C): resolve the optional `role` argument
        // against the typed `RoleTemplate` registry BEFORE we forward to
        // the native spawn delegate. Unknown values fail at the boundary
        // (structured error) rather than silently spawning under a role
        // the LLM did not ask for.
        //
        // PR #1177 codex round-2 P2 fix: also consult the Codex
        // `agent_type` alias path (`agent_type: "worker"` etc.) so a
        // caller that uses the historical #1148 alias instead of an
        // explicit `role` still gets the spawn-compatible
        // `allowed_tools` budget folded in AND the BackgroundTask
        // role/stamp populated. `normalize_spawn_agent_args_with_role`
        // already mirrors the agent_type alias to `role` for the wire
        // payload; this hoist mirrors it for the boundary-side
        // template injection too.
        // Issue #971 codex round-4 P3 (follow-up to PR #1177): trim AND
        // lowercase the caller-supplied role before lookup so case-
        // sloppy spellings (e.g. `role: "Reviewer"`, `role: " Reviewer "`,
        // or the display label models sometimes echo back) canonicalize
        // through the registry instead of returning `unknown_role`.
        // `RoleTemplate::for_name` is case-sensitive (the registry's
        // canonical names are all lower-case), so the normalization
        // has to happen at the boundary.
        //
        // Issue #971 codex round-5 P2 follow-up: also normalize space
        // and hyphen separators to underscore so display labels like
        // `"Test Worker"` / `"Test-Worker"` map to the canonical
        // `"test_worker"` key. The registry's canonical names use
        // snake_case; display labels use Title Case with spaces, and
        // models commonly echo back the display label. Without this
        // additional normalization, `role: "Test Worker"` would
        // lowercase to `"test worker"` and still miss the exact-name
        // lookup.
        let role_template = match args
            .get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .map(|s| s.to_ascii_lowercase().replace(['-', ' '], "_"))
        {
            Some(name) if !name.is_empty() => {
                match crate::role_template::RoleTemplate::for_name(&name) {
                    Some(template) => Some(template),
                    None => {
                        let registered: Vec<&str> = crate::role_template::RoleTemplate::all()
                            .iter()
                            .map(|tpl| tpl.name)
                            .collect();
                        return Ok(ToolResult {
                            output: format!(
                                "spawn_agent: unknown role {:?}; registered roles: {}",
                                name,
                                registered.join(", ")
                            ),
                            success: false,
                            structured_metadata: Some(json!({
                                "codex_tool": "spawn_agent",
                                "error": "unknown_role",
                                "role": name,
                                "registered_roles": registered,
                            })),
                            ..Default::default()
                        });
                    }
                }
            }
            _ => args
                .get("agent_type")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .and_then(crate::role_template::RoleTemplate::for_codex_agent_type),
        };

        let Some(delegate) = self.delegate.as_ref() else {
            return spawn_agent_without_delegate(args, ctx).await;
        };
        let spawn_args = normalize_spawn_agent_args_with_role(args, role_template);
        let before = ctx
            .task_supervisor
            .as_ref()
            .map(|supervisor| {
                supervisor
                    .get_all_tasks()
                    .into_iter()
                    .map(|task| task.id)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let result = delegate.execute_with_context(ctx, &spawn_args).await?;
        if !result.success {
            return Ok(result);
        }
        let task = ctx
            .task_supervisor
            .as_ref()
            .and_then(|supervisor| newest_spawned_task(supervisor, &before));
        let mut payload = json!({
            "status": "started",
            "output": result.output,
        });
        let mut resolved_task_id: Option<String> = None;
        if let Some(task) = task {
            payload["agent_id"] = json!(task.id);
            payload["status"] = json!(task.status.as_str());
            payload["runtime_state"] = json!(format!("{:?}", task.runtime_state));
            payload["child_session_key"] = json!(task.child_session_key);
            payload["terminal"] = json!(task.status.is_terminal());
            resolved_task_id = Some(task.id);
        }

        // Issue #971 (M14-C): label the supervisor's BackgroundTask with
        // the resolved role so the M13 `task/list` and `task/updated`
        // projections inherit `role = "reviewer"` (or whatever the
        // caller resolved). Without this the AppUI spawn-role badge
        // cannot render and the M14-C acceptance check ("child task
        // summaries and artifacts appear through M13 task/list") fails.
        //
        // Issue #971 codex iter-3 P2.2: also stamp a bounded
        // `runtime_policy_stamp` snapshot of the resolved template +
        // effective allow list, so reconnect hydration / `task/updated`
        // subscribers see the server-resolved sandbox / approval /
        // model preference / tool budget the child agent is running
        // under. Without the stamp, `task/list` would only carry the
        // role NAME and clients would have to re-resolve the template
        // registry to learn the effective policy — defeating the
        // M14-C acceptance "role/tool/sandbox/model policy resolved
        // by the server runtime".
        if let (Some(template), Some(task_id), Some(supervisor)) = (
            role_template,
            resolved_task_id.as_deref(),
            ctx.task_supervisor.as_ref(),
        ) {
            // Issue #971 codex iter-4 P2 + iter-5 P2: the
            // `allowed_tools` field IS effective when no manifest is
            // referenced (the spawn tool's child registry runs under
            // EXACTLY this allow list), but `sandbox_mode`,
            // `approval_policy`, and `model_preference` are advisory
            // — they're the template's DECLARED defaults, not the
            // sandbox/approval settings the spawned `SpawnTool`
            // actually applies. When `agent_definition_id` is set,
            // `SpawnTool::apply_agent_definition` may further prune
            // the allow list via the manifest's `tools` and
            // `disallowed_tools`, so the stamp marks the dimension as
            // `subject_to_manifest` and surfaces the manifest id —
            // otherwise `task/list` could report tools as `enforced`
            // allowed after the manifest pruned them.
            let manifest_id = spawn_args
                .get("agent_definition_id")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty());
            let effective_allowed: Vec<&str> = spawn_args
                .get("allowed_tools")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let allowed_tools_enforcement = if manifest_id.is_some() {
                "subject_to_manifest"
            } else {
                "enforced"
            };
            let mut stamp = json!({
                "role": template.name,
                "role_display_name": template.display_name,
                // Pre-manifest allow list. When a manifest is set,
                // the child's actual policy may be tighter — see
                // `policy_enforcement.allowed_tools`.
                "allowed_tools": effective_allowed,
                "policy_enforcement": {
                    "allowed_tools": allowed_tools_enforcement,
                    "sandbox_mode": "advisory",
                    "approval_policy": "advisory",
                    "model_preference": "advisory",
                },
                // Advisory: the role's declared defaults. The spawn
                // tool does not currently propagate these to the
                // child sandbox / approval / model resolution — they
                // ride as metadata so a future wiring can surface
                // the gap and so clients can render the role's
                // self-description without re-resolving the registry.
                "declared_sandbox_mode": template.default_sandbox_mode,
                "declared_approval_policy": template.default_approval_policy,
                "declared_model_preference": template.model_preference.as_str(),
            });
            if let Some(id) = manifest_id {
                stamp["agent_definition_id"] = json!(id);
            }
            supervisor.set_m13b_projection(
                task_id,
                Some("model".to_string()),
                Some(template.name.to_string()),
                None,
                None,
                Some(stamp),
            );
        }

        // PR #1177 reconciliation: the prefix prepending was moved
        // downstream into `spawn.rs::apply_role_template` to avoid
        // doubling the prefix in the child's system context. As a
        // result `spawn_args.additional_instructions` no longer
        // carries the server-owned prompt prefix at this layer, so
        // the dedicated redaction step from PR #1171 is unnecessary.
        let mut meta = json!({
            "codex_tool": "spawn_agent",
            "octos_tool": "spawn",
            "spawn_args": spawn_args.clone(),
        });
        if let Some(template) = role_template {
            meta["role"] = json!(template.name);
            meta["role_summary"] = serde_json::to_value(template.summary()).unwrap_or(Value::Null);
        }
        Ok(ToolResult {
            output: payload.to_string(),
            success: true,
            structured_metadata: Some(meta),
            ..Default::default()
        })
    }
}

async fn send_input_body(_: &dyn Tool, args: &Value, ctx: &ToolContext) -> Result<ToolResult> {
    let input: AgentTargetInput =
        serde_json::from_value(args.clone()).unwrap_or(AgentTargetInput {
            target: None,
            agent_id: None,
            targets: Vec::new(),
            timeout_ms: None,
        });
    let target = input
        .target
        .or(input.agent_id)
        .or_else(|| input.targets.into_iter().next());
    let Some(target) = target else {
        return Ok(ToolResult {
            output: "send_input requires agent_id or target".to_string(),
            success: false,
            ..Default::default()
        });
    };
    let Some(supervisor) = ctx.task_supervisor.as_ref() else {
        return Ok(ToolResult {
            output: "send_input requires a task supervisor in ToolContext".to_string(),
            success: false,
            ..Default::default()
        });
    };
    let Some(task) = supervisor.get_task(&target) else {
        return Ok(ToolResult {
            output: format!("unknown agent: {target}"),
            success: false,
            ..Default::default()
        });
    };

    let mut recorded = serde_json::Map::new();
    recorded.insert("agent_id".to_string(), Value::String(target.clone()));
    recorded.insert("request".to_string(), args.clone());
    recorded.insert("recorded_at".to_string(), json!(chrono::Utc::now()));
    let mut merged = serde_json::Map::new();
    if let Some(existing) = task.tool_input {
        merged.insert("original_tool_input".to_string(), existing);
    }
    merged.insert("last_codex_send_input".to_string(), Value::Object(recorded));
    supervisor.set_tool_input(&target, Value::Object(merged));

    Ok(ToolResult {
        output: json!({
            "ok": true,
            "agent_id": target,
            "status": task.status.as_str(),
            "recorded": true,
            "delivered": false,
            "message": "Input recorded on the supervised task. Live conversational delivery is not attached to this backend."
        })
        .to_string(),
        success: true,
        structured_metadata: Some(json!({
            "codex_tool": "send_input",
            "agent_id": target,
            "recorded": true,
            "delivery": "supervisor_metadata",
        })),
        ..Default::default()
    })
}

async fn resume_agent_body(_: &dyn Tool, args: &Value, ctx: &ToolContext) -> Result<ToolResult> {
    let input: AgentTargetInput =
        serde_json::from_value(args.clone()).unwrap_or(AgentTargetInput {
            target: None,
            agent_id: None,
            targets: Vec::new(),
            timeout_ms: None,
        });
    let target = input
        .target
        .or(input.agent_id)
        .or_else(|| input.targets.into_iter().next());
    let Some(target) = target else {
        return Ok(ToolResult {
            output: "resume_agent requires agent_id or target".to_string(),
            success: false,
            ..Default::default()
        });
    };
    let Some(supervisor) = ctx.task_supervisor.as_ref() else {
        return Ok(ToolResult {
            output: "resume_agent requires a task supervisor in ToolContext".to_string(),
            success: false,
            ..Default::default()
        });
    };
    match supervisor.relaunch(&target, RelaunchOpts::default()) {
        Ok(new_agent_id) => Ok(ToolResult {
            output: json!({
                "agent_id": target,
                "resumed_agent_id": new_agent_id,
                "status": "spawned"
            })
            .to_string(),
            success: true,
            ..Default::default()
        }),
        Err(TaskRelaunchError::StillActive) => Ok(ToolResult {
            output: json!({
                "agent_id": target,
                "status": "active",
                "message": "agent is already active"
            })
            .to_string(),
            success: true,
            ..Default::default()
        }),
        Err(TaskRelaunchError::NotFound) => Ok(ToolResult {
            output: format!("unknown agent: {target}"),
            success: false,
            ..Default::default()
        }),
    }
}

#[derive(Debug, Deserialize)]
struct AgentTargetInput {
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn collect_agent_statuses(
    supervisor: &crate::task_supervisor::TaskSupervisor,
    targets: &[String],
) -> (Vec<Value>, bool) {
    let statuses: Vec<Value> = targets
        .iter()
        .map(|target| match supervisor.get_task(target) {
            Some(task) => json!({
                "agent_id": target,
                "status": task.status.as_str(),
                "runtime_state": format!("{:?}", task.runtime_state),
                "terminal": task.status.is_terminal(),
                "error": task.error,
                "output_files": task.output_files,
                "child_session_key": task.child_session_key,
            }),
            None => json!({
                "agent_id": target,
                "status": "unknown",
                "terminal": true,
            }),
        })
        .collect();
    let all_terminal = statuses
        .iter()
        .all(|status| status["terminal"].as_bool().unwrap_or(true));
    (statuses, all_terminal)
}

async fn wait_agent_body(_: &dyn Tool, args: &Value, ctx: &ToolContext) -> Result<ToolResult> {
    let input: AgentTargetInput =
        serde_json::from_value(args.clone()).unwrap_or(AgentTargetInput {
            target: None,
            agent_id: None,
            targets: Vec::new(),
            timeout_ms: None,
        });
    let mut targets = input.targets;
    if let Some(target) = input.target.or(input.agent_id) {
        targets.push(target);
    }
    let Some(supervisor) = ctx.task_supervisor.as_ref() else {
        return Ok(ToolResult {
            output: "wait_agent requires a task supervisor in ToolContext".to_string(),
            success: false,
            ..Default::default()
        });
    };
    let timeout_ms = input.timeout_ms.unwrap_or(30_000).min(3_600_000);
    let started = Instant::now();
    let statuses = loop {
        let (statuses, all_terminal) = collect_agent_statuses(supervisor, &targets);
        if all_terminal || started.elapsed() >= Duration::from_millis(timeout_ms) {
            break statuses;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    Ok(ToolResult {
        output: json!({ "agents": statuses }).to_string(),
        success: true,
        ..Default::default()
    })
}

async fn close_agent_body(_: &dyn Tool, args: &Value, ctx: &ToolContext) -> Result<ToolResult> {
    let input: AgentTargetInput =
        serde_json::from_value(args.clone()).unwrap_or(AgentTargetInput {
            target: None,
            agent_id: None,
            targets: Vec::new(),
            timeout_ms: None,
        });
    let target = input
        .target
        .or(input.agent_id)
        .or_else(|| input.targets.into_iter().next());
    let Some(target) = target else {
        return Ok(ToolResult {
            output: "close_agent requires agent_id or target".to_string(),
            success: false,
            ..Default::default()
        });
    };
    let Some(supervisor) = ctx.task_supervisor.as_ref() else {
        return Ok(ToolResult {
            output: "close_agent requires a task supervisor in ToolContext".to_string(),
            success: false,
            ..Default::default()
        });
    };
    match supervisor.get_task(&target) {
        Some(task)
            if matches!(
                task.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            ) =>
        {
            Ok(ToolResult {
                output: json!({"agent_id": target, "status": task.status.as_str(), "closed": true})
                    .to_string(),
                success: true,
                ..Default::default()
            })
        }
        Some(_) => match supervisor.cancel(&target) {
            Ok(()) => Ok(ToolResult {
                output: json!({"agent_id": target, "status": "cancelled", "closed": true})
                    .to_string(),
                success: true,
                ..Default::default()
            }),
            Err(error) => Ok(ToolResult {
                output: format!("failed to close agent {target}: {error}"),
                success: false,
                ..Default::default()
            }),
        },
        None => Ok(ToolResult {
            output: format!("unknown agent: {target}"),
            success: false,
            ..Default::default()
        }),
    }
}

simple_codex_tool!(
    UpdatePlanTool,
    "update_plan",
    "Update the visible task plan for Codex-compatible coding workflows.",
    update_plan_body
);
simple_codex_tool!(
    RequestUserInputTool,
    "request_user_input",
    "Request structured user input from the host UI.",
    request_user_input_body
);
simple_codex_tool!(
    SendInputTool,
    "send_input",
    "Send input to a Codex-compatible subagent.",
    send_input_body
);
simple_codex_tool!(
    ResumeAgentTool,
    "resume_agent",
    "Resume a Codex-compatible subagent handle.",
    resume_agent_body
);
simple_codex_tool!(
    WaitAgentTool,
    "wait_agent",
    "Inspect or wait on Codex-compatible subagent handles.",
    wait_agent_body
);
simple_codex_tool!(
    CloseAgentTool,
    "close_agent",
    "Close or cancel a Codex-compatible subagent handle.",
    close_agent_body
);

// ---------------------------------------------------------------------------
// #1172 — Codex naming-parity aliases: `bash`, `delegate`.
//
// Codex CLI exposes these model-visible names alongside the canonical
// Octos surface (`shell` / `exec_command`, `spawn_agent` + `wait_agent`).
// A Codex-trained model emitting `bash(cmd=…)` or `delegate(role=…)`
// without these aliases hits "tool not found" first and recovers via
// `tool_search` on the retry. Registering the aliases removes that
// first-call round trip without changing the underlying capability set.
// ---------------------------------------------------------------------------

const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;
const MAX_BASH_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Deserialize)]
struct BashInput {
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    workdir: Option<String>,
}

/// Codex-compatible `bash` alias.
///
/// One-shot, non-PTY shell entrypoint. Mirrors Codex CLI 0.131.0's
/// `bash(cmd, timeout_ms?)` shape so a Codex-trained model lands on the
/// alias directly instead of probing `tool_search` first. Shares the
/// command policy + approval policy + sandbox with `exec_command` /
/// `shell` so a deny in one path denies in all three; the bash schema
/// is intentionally minimal (`cmd` + optional `timeout_ms`) to keep the
/// surface area visible to the model small.
pub struct BashTool {
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    policy: Arc<dyn CommandPolicy>,
    approval_policy: ApprovalPolicy,
    sandbox: Arc<dyn Sandbox>,
    /// #28d — the bash-file-writes knob, loaded ONCE at construction from
    /// the session's ToolPolicy (never re-read per call). Shared
    /// judge/escape-hatch with ShellTool — see `super::shell`.
    bash_file_writes: BashFileWrites,
}

impl BashTool {
    pub fn new(base_dir: impl Into<PathBuf>, sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            policy: Arc::new(crate::policy::SafePolicy::default()),
            approval_policy: ApprovalPolicy::Ask,
            sandbox,
            bash_file_writes: BashFileWrites::default(),
        }
    }

    /// #28d — set the bash file-writes knob (defaults to `allow`).
    pub fn with_bash_file_writes(mut self, mode: BashFileWrites) -> Self {
        self.bash_file_writes = mode;
        self
    }
    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    pub fn with_policy(mut self, policy: Arc<dyn CommandPolicy>) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_approval_policy(mut self, approval_policy: ApprovalPolicy) -> Self {
        self.approval_policy = approval_policy;
        self
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn description(&self) -> &str {
        "Run a one-shot shell command and return stdout/stderr. Non-PTY, non-interactive. Codex-compatible alias of exec_command / shell with a stripped-down schema (`cmd`, optional `timeout_ms`)."
    }

    fn tags(&self) -> &[&str] {
        &["runtime", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        // Same rationale as ShellTool / ExecCommandTool: filesystem-
        // mutating shell invocations cannot race other tool calls.
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cmd": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1000,
                    "maximum": MAX_BASH_TIMEOUT_SECS * 1000,
                    "description": "Optional timeout in milliseconds (default 120000)"
                },
                "workdir": {
                    "type": "string",
                    "description": "Optional working directory relative to the workspace"
                }
            },
            "required": ["cmd"]
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: BashInput =
            serde_json::from_value(args.clone()).wrap_err("invalid bash tool input")?;
        let Some(command) = input.cmd.clone().or_else(|| input.command.clone()) else {
            return Ok(ToolResult {
                output: "bash requires `cmd`".to_string(),
                success: false,
                ..Default::default()
            });
        };
        let cwd = match resolve_optional_workdir(
            &self.base_dir,
            input.workdir.as_deref(),
            self.filesystem_scope,
        ) {
            Ok(cwd) => cwd,
            Err(output) => {
                return Ok(ToolResult {
                    output,
                    success: false,
                    ..Default::default()
                });
            }
        };

        if let Some(result) = request_command_approval(
            self.name(),
            &command,
            &cwd,
            &self.policy,
            self.approval_policy,
        )
        .await
        {
            return Ok(result);
        }

        let timeout_secs = input
            .timeout_ms
            .map(|ms| ms.div_ceil(1000))
            .unwrap_or(DEFAULT_BASH_TIMEOUT_SECS)
            .clamp(1, MAX_BASH_TIMEOUT_SECS);

        // #28d — deny knob on the CODING bash path: same heuristic judge
        // and escape hatch as ShellTool (single shared implementation in
        // `super::shell`), loaded at construction. A refusal happens BEFORE
        // any spawn/snapshot.
        if self.bash_file_writes == BashFileWrites::Deny
            && !super::shell::command_allows_write_explicitly(&command)
            && super::shell::command_looks_like_file_write(&command)
        {
            return Ok(ToolResult {
                output: "Command refused by tool_policy.bash_file_writes=deny (it looks like a \
                         file-writing shell command). Use the edit_file / diff_edit tools for \
                         code changes instead. If this refusal is a false positive, append the \
                         comment `# octos:allow-write` to the command line to run it \
                         explicitly. Command: "
                    .to_owned()
                    + &command,
                success: false,
                ..Default::default()
            });
        }

        // #28c-r1 — receipt snapshot root: a leading literal `cd X &&`
        // prefix (the coding-session idiom) makes X the root; otherwise the
        // session workdir. Prevents the false `files_changed: 0` on writes
        // outside the workspace root (outer-loop live verdict).
        let (snapshot_root, receipt_scope) = receipt_scope_root(&cwd, &command);
        // #28c — BEFORE snapshot for the file-change receipt, reusing the
        // SAME shared 28a module as ShellTool. `None` on non-git/fail-open
        // omits the receipt.
        // Fail closed on an unhonorable sandbox config BEFORE spawning
        // anything: the typed refusal (with its per-OS remediation) IS the
        // tool result. The wrap-level refusal command would also fail, but a
        // background command discards its stderr, and the model deserves the
        // full remediation text, not a truncated stderr line.
        if let Some(refusal) = self.sandbox.refusal() {
            return Ok(ToolResult {
                output: refusal.to_string(),
                success: false,
                ..Default::default()
            });
        }

        let dirty_before = super::shell::snapshot_dirty_paths(&snapshot_root);
        let mut cmd = self.sandbox.wrap_command(&command, &cwd);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        // codex review (#1172) P2 (follow-up): put the child in its own
        // process group BEFORE spawn so the negative-PID kill below
        // reaches every grandchild it forked. Without this, a command
        // like `bash(cmd="(sleep 60; touch late) & wait", timeout_ms=1000)`
        // would leave the backgrounded `sleep` alive in the original
        // process group and the timeout cleanup would only kill the
        // wrapper shell. `process_group(0)` is the Unix-only knob; on
        // Windows job objects would be the analogue but `taskkill /F /T`
        // already walks the process tree.
        #[cfg(unix)]
        cmd.process_group(0);
        sanitize_command_env(&mut cmd, &EnvAllowlist::empty());
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Ok(ToolResult {
                    output: format!("Failed to execute command: {error}"),
                    success: false,
                    ..Default::default()
                });
            }
        };
        // codex review (#1172) P2: `tokio::process::Child` does NOT
        // kill the underlying process on drop, so a `timeout()` that
        // expires would leave the shell running and able to mutate
        // the workspace later. Save the PID before `wait_with_output`
        // takes ownership of the child, then on timeout send
        // SIGTERM -> brief grace -> SIGKILL (Unix) or `taskkill /F /T`
        // (Windows). Mirrors `ShellTool`'s kill-on-timeout path.
        let child_pid = child.id();
        // Armed for the whole wait: a dropped future (user interrupt ->
        // `agent_task.abort()`) reaches neither arm below. See `ChildGroupGuard`.
        let mut group_guard = ChildGroupGuard::new(child_pid);
        let waited = timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;
        group_guard.disarm();
        match waited {
            Ok(Ok(output)) => {
                let mut text = String::new();
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                if !output.stderr.is_empty() {
                    if !text.is_empty() {
                        text.push_str("\n--- stderr ---\n");
                    }
                    text.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                if text.is_empty() {
                    text.push_str("(no output)");
                }
                let exit_code = output.status.code().unwrap_or(-1);
                text.push_str(&format!("\n\nExit code: {exit_code}"));
                // #28c — file-change receipt appended ONCE to THIS result's
                // tail (28a semantics: prompt-cache stable, never a system
                // prompt / history rewrite). Non-git fail-open ⇒ None ⇒
                // omitted (acceptance ④ of the 28a set).
                if let Some(receipt) = super::shell::diff_to_receipt(
                    dirty_before,
                    super::shell::snapshot_dirty_paths(&snapshot_root),
                ) {
                    text.push_str(&receipt);
                    text.push_str(&format!("\nscope: {receipt_scope}"));
                    // #28d — warn knob: nudge ONLY when files actually
                    // changed (same after-snapshot as the receipt; no
                    // second scan). Zero behavior under `allow`.
                    if self.bash_file_writes == BashFileWrites::Warn
                        && !receipt.trim_end().ends_with("files_changed: 0")
                    {
                        text.push_str(
                            "\nnote: prefer the edit_file / diff_edit tools for code changes (tool_policy.bash_file_writes=warn)",
                        );
                    }
                }
                // Scan BEFORE truncation, append AFTER — see run_to_completion.
                let hint =
                    sandbox_denial_hint(!self.sandbox.is_noop(), output.status.success(), &text);
                let mut out = truncate_output(text, MAX_CAPTURE_BYTES);
                if let Some(hint) = hint {
                    out.push_str(hint);
                }
                Ok(ToolResult {
                    output: out,
                    success: output.status.success(),
                    structured_metadata: Some(json!({
                        "codex_tool": "bash",
                        "octos_tool": "exec_command",
                        "exit_code": exit_code,
                    })),
                    ..Default::default()
                })
            }
            Ok(Err(error)) => Ok(ToolResult {
                output: format!("Failed to execute command: {error}"),
                success: false,
                ..Default::default()
            }),
            Err(_) => {
                kill_timed_out_child(child_pid).await;
                Ok(ToolResult {
                    output: format!("Command timed out after {timeout_secs} seconds"),
                    success: false,
                    ..Default::default()
                })
            }
        }
    }
}

/// Kills a child's process group if dropped while still armed.
///
/// [`kill_timed_out_child`] only runs on the TIMEOUT arm of the `match` below.
/// When the whole future is dropped instead, neither that arm nor tokio's own
/// cleanup fires — `tokio::process::Child` does not kill on drop — and the
/// child's entire process group survives.
///
/// That is not a hypothetical path: it is exactly what a user interrupt does.
/// Esc on the serve path calls `agent_task.abort()`, which drops this future,
/// so `bash("npm run dev")` kept running after the UI said the turn had
/// stopped — holding its ports and still able to write to the workspace. The
/// guard closes that by making cleanup a property of the scope rather than of
/// one match arm.
///
/// Disarm on every path that has already reaped or killed the child.
struct ChildGroupGuard(Option<u32>);

impl ChildGroupGuard {
    fn new(child_pid: Option<u32>) -> Self {
        Self(child_pid)
    }

    /// The normal paths (clean exit, spawn error, timeout ladder) have already
    /// dealt with the child; leave it alone.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ChildGroupGuard {
    fn drop(&mut self) {
        let Some(pid) = self.0 else {
            return;
        };
        // `Drop` cannot await, but the graceful ladder needs a grace period
        // between SIGTERM and SIGKILL. Hand it to a detached task when a
        // runtime is still available — cancellation drops this guard while the
        // runtime is very much alive, so that is the normal case. Only if there
        // is no runtime (shutdown) fall back to a synchronous hard kill, which
        // is a better end state than leaking the group.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(kill_timed_out_child(Some(pid)));
            }
            Err(_) => {
                #[cfg(unix)]
                {
                    signal_process_group(pid, "-9");
                    signal_process(pid, "-9");
                }
                #[cfg(windows)]
                {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &pid.to_string()])
                        .status();
                }
            }
        }
    }
}

/// Best-effort kill of a child whose `wait_with_output()` was dropped
/// when a `tokio::time::timeout` expired. Mirrors `ShellTool`'s
/// kill-on-timeout: SIGTERM the process group/tree, brief grace period,
/// then SIGKILL if any process remains. On Windows uses `taskkill /F /T`.
/// Errors are swallowed because the call is best-effort cleanup —
/// the timeout result has already been returned to the caller.
async fn kill_timed_out_child(child_pid: Option<u32>) {
    let Some(pid) = child_pid else {
        return;
    };
    #[cfg(unix)]
    {
        let descendants = collect_descendant_pids(pid);
        signal_process_group(pid, "-15");
        signal_pids(&descendants, "-15");
        signal_process(pid, "-15");

        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut remaining = descendants;
        remaining.extend(collect_descendant_pids(pid));
        remaining.sort_unstable();
        remaining.dedup();

        if process_group_exists(pid)
            || process_exists(pid)
            || remaining
                .iter()
                .any(|descendant| process_exists(*descendant))
        {
            signal_process_group(pid, "-9");
            signal_pids(&remaining, "-9");
            signal_process(pid, "-9");
        }
    }
    #[cfg(windows)]
    {
        use std::process::Command as StdCommand;
        let _ = StdCommand::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .status();
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: &str) -> bool {
    use std::process::Command as StdCommand;

    let group = format!("-{pid}");
    StdCommand::new("kill")
        .args([signal, "--", &group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn signal_process(pid: u32, signal: &str) -> bool {
    use std::process::Command as StdCommand;

    StdCommand::new("kill")
        .args([signal, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn signal_pids(pids: &[u32], signal: &str) {
    for pid in pids {
        signal_process(*pid, signal);
    }
}

#[cfg(unix)]
fn process_group_exists(pid: u32) -> bool {
    signal_process_group(pid, "-0")
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    signal_process(pid, "-0")
}

#[cfg(unix)]
fn collect_descendant_pids(root_pid: u32) -> Vec<u32> {
    use std::process::Command as StdCommand;

    let output = match StdCommand::new("ps").args(["-eo", "pid=,ppid="]).output() {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    let mut children_by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        children_by_parent.entry(ppid).or_default().push(pid);
    }

    let mut descendants = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = children_by_parent
        .get(&root_pid)
        .cloned()
        .unwrap_or_default();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        descendants.push(pid);
        if let Some(children) = children_by_parent.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }
    descendants
}

#[derive(Debug, Deserialize)]
struct DelegateInput {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

const DEFAULT_DELEGATE_TIMEOUT_MS: u64 = 300_000;
const MAX_DELEGATE_TIMEOUT_MS: u64 = 3_600_000;

/// Codex-compatible `delegate` one-call wrapper.
///
/// Chains `spawn_agent` (start a supervised subagent) → `wait_agent`
/// (block until terminal) → extract result. Codex CLI exposes this as
/// a convenience for clients that don't want to manage the
/// spawn/wait/close lifecycle by hand.
///
/// `role` is resolved through the M14-C `RoleTemplate` registry when
/// the name matches one of the four canonical roles (`reviewer`,
/// `implementer`, `test_worker`, `explorer`); unknown role names are
/// rejected at the tool boundary so a typo (`"review"` vs `"reviewer"`)
/// surfaces immediately instead of silently smuggling an unbounded
/// prompt through.
///
/// Wiring note (#971 partial): the role template's `prompt_prefix` and
/// `allowed_tools` budget are folded into the spawn arguments here, but
/// concrete sandbox / approval policy enforcement still flows through
/// the underlying `spawn_agent` delegate. When #971 fully lands, this
/// tool will pick up policy gating "for free" through the upgraded
/// spawn_agent contract.
pub struct DelegateAliasTool {
    spawn_agent: Option<Arc<dyn Tool>>,
}

impl Default for DelegateAliasTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DelegateAliasTool {
    pub fn new() -> Self {
        Self { spawn_agent: None }
    }

    pub fn with_spawn_agent(spawn_agent: Arc<dyn Tool>) -> Self {
        Self {
            spawn_agent: Some(spawn_agent),
        }
    }
}

#[async_trait]
impl Tool for DelegateAliasTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Spawn a supervised Codex-compatible subagent, wait for it to finish, and return the result + artifacts. One-call wrapper around `spawn_agent` + `wait_agent`. `role` resolves through the M14-C role template registry (reviewer / implementer / test_worker / explorer)."
    }

    fn tags(&self) -> &[&str] {
        &["gateway", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "role": {
                    "type": "string",
                    "description": "Subagent role: reviewer, implementer, test_worker, or explorer"
                },
                "task": {
                    "type": "string",
                    "description": "Task description for the subagent"
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1000,
                    "maximum": MAX_DELEGATE_TIMEOUT_MS,
                    "description": "How long to wait for the child to terminate (default 300000)"
                }
            },
            "required": ["role", "task"]
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let input: DelegateInput =
            serde_json::from_value(args.clone()).wrap_err("invalid delegate input")?;
        let Some(role) = input
            .role
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(ToolResult {
                output: "delegate requires `role`".to_string(),
                success: false,
                structured_metadata: Some(json!({
                    "codex_tool": "delegate",
                    "error_kind": "coding_tool_denied",
                    "reason": "missing_role",
                })),
                ..Default::default()
            });
        };
        let Some(task) = input
            .task
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(ToolResult {
                output: "delegate requires `task`".to_string(),
                success: false,
                structured_metadata: Some(json!({
                    "codex_tool": "delegate",
                    "error_kind": "coding_tool_denied",
                    "reason": "missing_task",
                })),
                ..Default::default()
            });
        };
        // Resolve role through the typed registry. Unknown role names
        // are rejected at the boundary so drift (`"review"` vs
        // `"reviewer"`) doesn't silently smuggle an undeclared prompt
        // prefix through.
        let template = match crate::role_template::RoleTemplate::for_name(role) {
            Some(template) => template,
            None => {
                let canonical: Vec<&str> = crate::role_template::RoleTemplate::all()
                    .iter()
                    .map(|tpl| tpl.name)
                    .collect();
                return Ok(ToolResult {
                    output: format!(
                        "delegate: unknown role {role:?}; canonical roles: {canonical:?}"
                    ),
                    success: false,
                    structured_metadata: Some(json!({
                        "codex_tool": "delegate",
                        "error_kind": "coding_tool_denied",
                        "reason": "unknown_role",
                        "role": role,
                        "canonical_roles": canonical,
                    })),
                    ..Default::default()
                });
            }
        };

        let Some(spawn_agent) = self.spawn_agent.as_ref() else {
            return Ok(ToolResult {
                output: "delegate requires the session runtime to register a spawn_agent delegate"
                    .to_string(),
                success: false,
                structured_metadata: Some(json!({
                    "codex_tool": "delegate",
                    "error_kind": "coding_tool_missing",
                    "reason": "spawn_agent_unbound",
                })),
                ..Default::default()
            });
        };

        let Some(supervisor) = ctx.task_supervisor.as_ref().cloned() else {
            return Ok(ToolResult {
                output: "delegate requires a task supervisor in ToolContext".to_string(),
                success: false,
                structured_metadata: Some(json!({
                    "codex_tool": "delegate",
                    "error_kind": "coding_tool_missing",
                    "reason": "supervisor_unbound",
                })),
                ..Default::default()
            });
        };

        // Issue #971 codex round-5 P2 fix: the prior `delegate`
        // implementation manually prepended `template.prompt_prefix`
        // to the task here AND forwarded `role` to `spawn_agent`,
        // which then prepended the same prefix again through
        // `spawn.rs::apply_role_template` — the child received the
        // role's guardrails twice. Forwarding `role` is enough; the
        // native delegate path is the authoritative single source of
        // truth for the prefix prepend.
        let task_prompt = task.to_string();
        // Issue #971 codex round-4 P2 (follow-up to PR #1177): the
        // `delegate` wrapper used to ship raw `template.allowed_tools`
        // (a slice containing `group:*` identifiers) inside `spawn_args`.
        // The downstream `spawn_agent` boundary treats any non-empty
        // `allowed_tools` array as an inline override and SKIPS the
        // `to_spawn_compatible_allow()` expansion, so the raw group
        // entries reached the native `SpawnTool::ensure_subagent_tools_available`
        // check and every `delegate({"role": <runtime/read-only role>})`
        // call failed with "required tool not available: group:search".
        // Pre-expand here so the override path receives the same
        // spawn-compatible, exact-name list the explicit-role path
        // gets.
        let spawn_args = json!({
            "task": task_prompt,
            "label": format!("delegate-{}", template.name),
            "mode": "background",
            "role": template.name,
            "allowed_tools": template.to_spawn_compatible_allow(),
        });

        let before: HashSet<String> = supervisor
            .get_all_tasks()
            .into_iter()
            .map(|task| task.id)
            .collect();
        let spawn_result = spawn_agent.execute_with_context(ctx, &spawn_args).await?;
        if !spawn_result.success {
            return Ok(ToolResult {
                output: format!("delegate: spawn_agent failed: {}", spawn_result.output),
                success: false,
                structured_metadata: Some(json!({
                    "codex_tool": "delegate",
                    "error_kind": "coding_tool_missing",
                    "reason": "spawn_agent_failed",
                    "role": template.name,
                })),
                ..Default::default()
            });
        };
        let Some(task_record) = newest_spawned_task(&supervisor, &before) else {
            return Ok(ToolResult {
                output: "delegate: spawn_agent did not register a new task with the supervisor"
                    .to_string(),
                success: false,
                structured_metadata: Some(json!({
                    "codex_tool": "delegate",
                    "error_kind": "coding_tool_missing",
                    "reason": "spawn_agent_no_task",
                    "role": template.name,
                })),
                ..Default::default()
            });
        };
        let agent_id = task_record.id;

        // Block until the child reaches a terminal lifecycle state or
        // the caller-specified timeout fires. Mirrors `wait_agent_body`
        // so the contract stays identical.
        let timeout_ms = input
            .timeout_ms
            .unwrap_or(DEFAULT_DELEGATE_TIMEOUT_MS)
            .min(MAX_DELEGATE_TIMEOUT_MS);
        let started = Instant::now();
        let final_task = loop {
            let snapshot = supervisor.get_task(&agent_id);
            let is_terminal = snapshot
                .as_ref()
                .is_some_and(|task| task.status.is_terminal());
            if is_terminal || started.elapsed() >= Duration::from_millis(timeout_ms) {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        let (status_str, result_text, artifacts, error_text, terminal, child_session_key) =
            match final_task {
                Some(task) => (
                    task.status.as_str().to_string(),
                    task.summary.clone().unwrap_or_default(),
                    task.output_files.clone(),
                    task.error.clone(),
                    task.status.is_terminal(),
                    task.child_session_key.clone(),
                ),
                None => (
                    "unknown".to_string(),
                    String::new(),
                    Vec::new(),
                    Some(format!("agent {agent_id} not found in supervisor")),
                    true,
                    None,
                ),
            };
        let timed_out = !terminal && status_str != "unknown";
        let success = terminal && status_str == TaskStatus::Completed.as_str();
        let payload = json!({
            "agent_id": agent_id,
            "role": template.name,
            "status": status_str,
            "result": result_text,
            "artifacts": artifacts,
            "error": error_text,
            "terminal": terminal,
            "timed_out": timed_out,
            "child_session_key": child_session_key,
        });
        Ok(ToolResult {
            output: payload.to_string(),
            success,
            structured_metadata: Some(json!({
                "codex_tool": "delegate",
                "role": template.name,
                "agent_id": agent_id,
                "artifacts": artifacts,
                "terminal": terminal,
            })),
            ..Default::default()
        })
    }
}

// ---------------------------------------------------------------------------
// #972 / M14-B P1 tools: `view_image`, `tool_search`, `tool_suggest`.
//
// These complete the Codex-compatible coding tool surface declared by
// UPCR-2026-020. They resolve through the server-owned profile runtime
// (registered via `ToolRegistry::with_builtins`), respect the active
// `FilesystemScope` and `FileAccessMode`, and emit structured metadata so
// the AppUI tool contract can advertise them as `available`.
// ---------------------------------------------------------------------------

/// Snapshot entry exposed to `tool_search` / `tool_suggest`.
///
/// Built by [`ToolRegistry`] after every other builtin tool has registered, so
/// dynamic-discovery results reflect the *effective* coding tool contract for
/// the active profile (post policy / context / deferred filters can be applied
/// by the caller before constructing the snapshot).
#[derive(Debug, Clone)]
pub struct ToolCatalogEntry {
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
}

impl ToolCatalogEntry {
    pub fn new(name: impl Into<String>, description: impl Into<String>, tags: Vec<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            tags,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ViewImageInput {
    #[serde(default)]
    path: Option<String>,
}

/// Codex-compatible `view_image` tool.
///
/// Reads an image file from the workspace (respecting `FilesystemScope` and
/// `FileAccessMode`), detects the format from the magic header bytes, and
/// returns a structured metadata envelope the AppUI image-view flow can render
/// without re-reading the file. The tool intentionally does NOT inline the raw
/// image bytes — the host UI fetches them through the workspace artifact
/// channel.
pub struct ViewImageTool {
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    file_access: FileAccessMode,
}

impl ViewImageTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadWrite,
        }
    }

    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    pub fn with_file_access(mut self, file_access: FileAccessMode) -> Self {
        self.file_access = file_access;
        self
    }
}

/// Detected image format reported back to the model. Recognized purely from
/// magic header bytes — no `image` crate dependency is pulled in, which keeps
/// the tool surface free of binary parsing risk.
fn detect_image_format(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(("png", "image/png"));
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(("jpeg", "image/jpeg"));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(("gif", "image/gif"));
    }
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Some(("webp", "image/webp"));
    }
    if bytes.starts_with(b"BM") {
        return Some(("bmp", "image/bmp"));
    }
    // Plain SVG (no XML preamble) and SVG with an `<?xml` preamble. We sniff
    // by scanning a short prefix — SVGs in the wild routinely include a
    // comment block before the opening `<svg`.
    let prefix = bytes.get(..256.min(bytes.len())).unwrap_or(bytes);
    if std::str::from_utf8(prefix)
        .ok()
        .is_some_and(|text| text.contains("<svg"))
    {
        return Some(("svg", "image/svg+xml"));
    }
    None
}

#[async_trait]
impl Tool for ViewImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn description(&self) -> &str {
        "Inspect a local image file (PNG / JPEG / GIF / WEBP / BMP / SVG). Returns format, MIME type, and byte length so the host UI can render a preview."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path to the image"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        // `view_image` is read-only; both ReadOnly and ReadWrite modes permit
        // reads. The field is held for symmetry with other file tools and so
        // a future write-only mode can deny here without an API break.
        let _ = self.file_access;
        let input: ViewImageInput =
            serde_json::from_value(args.clone()).wrap_err("invalid view_image input")?;
        let Some(path) = input
            .path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            return Ok(ToolResult {
                output: "view_image requires `path`".to_string(),
                success: false,
                ..Default::default()
            });
        };
        let resolved =
            match super::resolve_path_with_scope(&self.base_dir, path, self.filesystem_scope) {
                Ok(resolved) => resolved,
                Err(_) => {
                    return Ok(ToolResult {
                        output: format!(
                            "view_image: path outside allowed filesystem scope: {path}"
                        ),
                        success: false,
                        structured_metadata: Some(json!({
                            "codex_tool": "view_image",
                            "error_kind": "coding_tool_denied",
                            "path": path,
                        })),
                        ..Default::default()
                    });
                }
            };
        // #1148 codex P2: open with O_NOFOLLOW (Unix) and read only a
        // bounded header prefix. `resolve_path_with_scope` returns a
        // LEXICAL path; the previous `tokio::fs::read` followed
        // symlinks AND read the entire file, which (a) bypasses
        // workspace symlink protection and (b) allocates a huge
        // buffer just to sniff magic bytes. SVG detection needs the
        // longest prefix (256 bytes); 512 gives headroom. `metadata`
        // surfaces the true byte length without reading.
        //
        // #1151: pass the workspace root so the helper can walk
        // ancestors and reject any parent symlink — `O_NOFOLLOW`
        // only catches a symlink AT the final path component, so a
        // symlinked PARENT directory (`workspace/link -> /outside`)
        // would otherwise let `view_image` read outside the workspace.
        //
        // #1153 codex P2: host-scope (DangerFullAccess) callers
        // legitimately read paths outside the workspace. The
        // ancestor walk's workspace stop would never be reached for
        // e.g. `/tmp/foo.png` on macOS — the walk would refuse `/tmp`
        // (which is a symlink on macOS). Pass None to skip the walk
        // for host scope; the Unix `O_NOFOLLOW` leaf guard below still
        // protects the final-component symlink case.
        let ancestor_stop: Option<&std::path::Path> = match self.filesystem_scope {
            FilesystemScope::Workspace => Some(self.base_dir.as_path()),
            FilesystemScope::Host => None,
        };
        let (bytes, byte_length) = match read_image_header_no_follow(&resolved, ancestor_stop) {
            Ok(pair) => pair,
            Err(error) => {
                return Ok(ToolResult {
                    output: format!("view_image: failed to read {path}: {error}"),
                    success: false,
                    structured_metadata: Some(json!({
                        "codex_tool": "view_image",
                        "error_kind": "coding_tool_missing",
                        "path": path,
                    })),
                    ..Default::default()
                });
            }
        };
        let (format, mime) = match detect_image_format(&bytes) {
            Some(pair) => pair,
            None => {
                return Ok(ToolResult {
                    output: format!(
                        "view_image: {path} does not match a recognised image header (PNG / JPEG / GIF / WEBP / BMP / SVG)"
                    ),
                    success: false,
                    structured_metadata: Some(json!({
                        "codex_tool": "view_image",
                        "error_kind": "coding_tool_denied",
                        "reason": "unrecognised_image_format",
                        "path": path,
                    })),
                    ..Default::default()
                });
            }
        };
        Ok(ToolResult {
            output: json!({
                "path": path,
                "format": format,
                "mime_type": mime,
                "byte_length": byte_length,
            })
            .to_string(),
            success: true,
            structured_metadata: Some(json!({
                "codex_tool": "view_image",
                "path": path,
                "format": format,
                "mime_type": mime,
                "byte_length": byte_length,
            })),
            ..Default::default()
        })
    }
}

/// #1148 codex P2: bounded-read helper for `view_image` that refuses
/// to follow symlinks. Reads only the first 512 bytes for magic-byte
/// detection — SVG sniffing scans up to 256, the binary formats all
/// need ≤12. The total file size is returned separately from
/// `metadata()` so callers can surface `byte_length` without reading
/// the whole file.
///
/// #1151: the original implementation had two symlink gaps:
///
///   1. **Unix:** `O_NOFOLLOW` only refuses a symlink at the FINAL
///      path component. `resolve_path_with_scope` is lexical, so a
///      symlinked PARENT directory (`workspace/link -> /outside/`)
///      would pass scope resolution and the open would follow the
///      parent symlink — `view_image` could read outside the
///      workspace.
///   2. **Windows:** `OpenOptions::open` already followed any
///      symlink/reparse point by the time the post-open
///      `file.metadata().is_symlink()` check ran. The check was
///      silently a no-op.
///
/// Both gaps are closed by walking ancestors from `resolved` up to
/// the configured `workspace_root` and calling `symlink_metadata` on
/// each — refusing if any ancestor (including the leaf) is a
/// symlink/reparse point. The walk stops at the workspace root
/// (inclusive) so we never traverse system roots. The Unix
/// `O_NOFOLLOW` flag is retained as defense in depth for the leaf.
///
/// `workspace_root` is `Some(path)` for workspace-scoped callers
/// (the ancestor walk stops at that path); pass `None` for host-
/// scoped callers (DangerFullAccess `FilesystemScope::Host`), where
/// the resolved path can legitimately live outside the workspace —
/// in that case ancestors like `/tmp` (a symlink on macOS) MUST NOT
/// reject the read. Codex review on #1153 caught this regression:
/// without the `Option` the workspace stop was never reached for a
/// host path so the walk hit `/tmp` and refused. Host-scope callers
/// still get the Unix `O_NOFOLLOW` leaf guard below as defense in
/// depth against the final-component symlink.
fn read_image_header_no_follow(
    resolved: &std::path::Path,
    workspace_root: Option<&std::path::Path>,
) -> std::io::Result<(Vec<u8>, u64)> {
    use std::io::Read;
    const HEADER_BYTES: usize = 512;

    // Pre-open ancestor walk: refuse any symlink/reparse-point in the
    // path between the workspace root and the leaf (inclusive). This
    // closes the Unix parent-symlink gap AND the Windows post-open
    // gap in one shot. The leaf check also acts as the Windows
    // symlink rejection (Unix still has O_NOFOLLOW below).
    //
    // Skipped entirely for host-scope (workspace_root=None) because
    // the resolved path is outside the workspace and the walk would
    // hit system symlinks (e.g. `/tmp` on macOS).
    //
    // #1153 codex P2 rev2: when we skip the ancestor walk for host
    // scope, the WINDOWS leaf-symlink guard goes with it. Unix still
    // has O_NOFOLLOW below, but the `#[cfg(not(unix))]` open has no
    // replacement. Keep at least a leaf-only `symlink_metadata` check
    // so a host symlink like `C:\tmp\link.png -> C:\secret\real.png`
    // doesn't quietly follow on Windows.
    match workspace_root {
        Some(root) => reject_symlink_ancestors(resolved, root)?,
        None => reject_leaf_symlink(resolved)?,
    }

    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(resolved)?
    };
    #[cfg(not(unix))]
    let file = std::fs::OpenOptions::new().read(true).open(resolved)?;

    let metadata = file.metadata()?;
    let byte_length = metadata.len();
    let mut reader = file.take(HEADER_BYTES as u64);
    let mut header = Vec::with_capacity(HEADER_BYTES.min(byte_length as usize));
    reader.read_to_end(&mut header)?;
    Ok((header, byte_length))
}

/// Leaf-only symlink check for host-scope reads. Equivalent to the
/// final iteration of `reject_symlink_ancestors` but without walking
/// upward — host scope intentionally accepts paths outside the
/// workspace, so we can't pick an ancestor stop.
///
/// On Unix this is belt-and-suspenders with the `O_NOFOLLOW` flag
/// used in the open below (both reject a symlinked leaf). On Windows
/// it's the ONLY leaf no-follow guard.
///
/// `NotFound` is propagated as `Ok(())` so the subsequent open
/// surfaces the real error rather than masking it as PermissionDenied.
fn reject_leaf_symlink(resolved: &std::path::Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(resolved) {
        Ok(meta) if meta.file_type().is_symlink() => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to follow symlink leaf: {}", resolved.display()),
        )),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Walk every ancestor of `resolved` (including `resolved` itself)
/// and refuse if any one is a symlink or Windows reparse point.
/// Stops at `workspace_root` (inclusive) so we never recurse into
/// system roots. Returns `Ok(())` when none of the inspected entries
/// are symlinks; returns `PermissionDenied` with a descriptive
/// message when any are.
///
/// Safety properties:
///
/// * Uses `symlink_metadata`, which does NOT follow the link, so a
///   symlinked ancestor is correctly classified.
/// * Terminates at the workspace root even if `resolved` does not
///   actually live under it (in which case the walk runs out of
///   ancestors and returns `Ok(())` — containment was already
///   checked by `resolve_path_with_scope`).
/// * Hard-bounded by `Path::ancestors`, which is finite.
fn reject_symlink_ancestors(
    resolved: &std::path::Path,
    workspace_root: &std::path::Path,
) -> std::io::Result<()> {
    for ancestor in resolved.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "refusing to follow symlink ancestor: {}",
                            ancestor.display()
                        ),
                    ));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // The leaf may not exist yet — keep walking up so a
                // symlinked PARENT still gets caught. The actual
                // open below will surface NotFound for the leaf.
            }
            Err(err) => return Err(err),
        }
        // Stop walking once we hit (and have inspected) the
        // configured workspace root. Going further would inspect
        // system directories that the caller has no jurisdiction
        // over.
        if ancestor == workspace_root {
            break;
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ToolSearchInput {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ToolSuggestInput {
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

const DEFAULT_DYNAMIC_DISCOVERY_LIMIT: usize = 8;
const MAX_DYNAMIC_DISCOVERY_LIMIT: usize = 32;

/// Codex-compatible `tool_search` tool.
///
/// Returns model-visible tools matching a substring query (case insensitive).
/// Backed by a snapshot of the active registry passed in at registration time,
/// which lets the discovery surface reflect the per-profile tool contract
/// without giving the tool a live `ToolRegistry` reference (which would be
/// reentrancy-hostile from inside `execute`).
pub struct ToolSearchTool {
    // #1148 codex P2: live shared catalog cell owned by the registry.
    // Updated on every registry mutation (via `refresh_live_catalog`)
    // so the discovery surface always reflects post-mutation visible
    // tools, including ones registered AFTER `with_builtins`
    // (chat/gateway/profile setup, MCP/plugin/pipeline/memory paths).
    catalog: Arc<std::sync::Mutex<Vec<ToolCatalogEntry>>>,
}

impl ToolSearchTool {
    pub fn new(catalog: Arc<std::sync::Mutex<Vec<ToolCatalogEntry>>>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self) -> &str {
        "Search the active coding tool contract for tools whose name or description matches a query. Returns ranked matches with `name`, `description`, and `tags`."
    }

    fn tags(&self) -> &[&str] {
        &["code", "discovery"]
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Free-form search query (case insensitive)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_DYNAMIC_DISCOVERY_LIMIT,
                    "description": "Maximum number of matches to return (default 8)"
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: ToolSearchInput =
            serde_json::from_value(args.clone()).wrap_err("invalid tool_search input")?;
        let query = input
            .query
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_lowercase();
        let limit = input
            .limit
            .unwrap_or(DEFAULT_DYNAMIC_DISCOVERY_LIMIT)
            .clamp(1, MAX_DYNAMIC_DISCOVERY_LIMIT);
        // #1148 codex P2: snapshot the live catalog under the
        // shared Mutex at execute time so we see post-mutation
        // visible tools.
        let catalog_snapshot: Vec<ToolCatalogEntry> = self
            .catalog
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let matches = search_catalog(&catalog_snapshot, &query, limit);
        let results: Vec<Value> = matches
            .iter()
            .map(|entry| {
                json!({
                    "name": entry.name,
                    "description": entry.description,
                    "tags": entry.tags,
                })
            })
            .collect();
        Ok(ToolResult {
            output: json!({
                "query": query,
                "matches": results,
                "total": catalog_snapshot.len(),
            })
            .to_string(),
            success: true,
            structured_metadata: Some(json!({
                "codex_tool": "tool_search",
                "query": query,
                "matches": results,
            })),
            ..Default::default()
        })
    }
}

/// Codex-compatible `tool_suggest` tool.
///
/// Given a free-form task description, returns a ranked list of tools likely
/// to be useful. Ranking is a deterministic keyword-overlap heuristic over
/// name + description + tags so we ship a useful default without smuggling an
/// LLM behind a tool call. Hosts that want richer ranking can replace the
/// implementation; the model-visible contract (input schema, output shape)
/// stays stable.
pub struct ToolSuggestTool {
    // #1148 codex P2: live shared catalog cell — see `ToolSearchTool`.
    catalog: Arc<std::sync::Mutex<Vec<ToolCatalogEntry>>>,
}

impl ToolSuggestTool {
    pub fn new(catalog: Arc<std::sync::Mutex<Vec<ToolCatalogEntry>>>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for ToolSuggestTool {
    fn name(&self) -> &str {
        "tool_suggest"
    }

    fn description(&self) -> &str {
        "Suggest tools for a free-form task description. Returns up to N ranked tools from the active coding tool contract."
    }

    fn tags(&self) -> &[&str] {
        &["code", "discovery"]
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Free-form description of the task you want a tool for"
                },
                "query": {
                    "type": "string",
                    "description": "Alias for `task`. Either field is accepted."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_DYNAMIC_DISCOVERY_LIMIT,
                    "description": "Maximum number of suggestions to return (default 8)"
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: ToolSuggestInput =
            serde_json::from_value(args.clone()).wrap_err("invalid tool_suggest input")?;
        let raw = input.task.or(input.query).unwrap_or_default();
        let task = raw.trim().to_lowercase();
        let limit = input
            .limit
            .unwrap_or(DEFAULT_DYNAMIC_DISCOVERY_LIMIT)
            .clamp(1, MAX_DYNAMIC_DISCOVERY_LIMIT);
        // #1148 codex P2: snapshot the live catalog under the
        // shared Mutex at execute time.
        let catalog_snapshot: Vec<ToolCatalogEntry> = self
            .catalog
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let suggestions = suggest_catalog(&catalog_snapshot, &task, limit);
        let results: Vec<Value> = suggestions
            .iter()
            .map(|(entry, score)| {
                json!({
                    "name": entry.name,
                    "description": entry.description,
                    "tags": entry.tags,
                    "score": score,
                })
            })
            .collect();
        Ok(ToolResult {
            output: json!({
                "task": task,
                "suggestions": results,
                "total": catalog_snapshot.len(),
            })
            .to_string(),
            success: true,
            structured_metadata: Some(json!({
                "codex_tool": "tool_suggest",
                "task": task,
                "suggestions": results,
            })),
            ..Default::default()
        })
    }
}

// ---------------------------------------------------------------------------
// #1149 / M14-B P2 tool: `image_generation`.
//
// Codex's optional image-generation surface. Octos doesn't ship a native
// image-generation backend yet (no MoFA media skill is bundled and the
// `octos-llm` providers — Anthropic / Gemini / OpenRouter — don't expose
// an image-generation endpoint; OpenAI does via DALL-E but isn't wired
// through `LlmProvider` either). Rather than leave the canonical Codex
// name unregistered (which would surface to the model as "tool not
// found"), we register a stub that returns a typed
// `coding_tool_unsupported` envelope. This keeps the wire-level contract
// complete: model-visible name advertised, structured error returned,
// follow-up work tracked in #1149 for a real backend (OpenAI image API
// or a bundled skill).
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ImageGenerationInput {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    n: Option<u32>,
}

/// Codex-compatible `image_generation` tool.
///
/// Stub: always returns a structured `coding_tool_unsupported` envelope. The
/// canonical Codex input shape (`prompt`, optional `size`, optional `n`) is
/// accepted and validated so a future backend-bound implementation can
/// upgrade in place without breaking the model-visible schema. See #1149
/// for the follow-up wiring (OpenAI image API or bundled skill).
pub struct ImageGenerationTool {
    backend_bound: bool,
}

impl Default for ImageGenerationTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageGenerationTool {
    /// Construct the stub variant. Always returns `coding_tool_unsupported`
    /// because no native or skill backend is bound. The constructor is kept
    /// `pub` so the `with_builtins` path and tests both reach it through one
    /// entrypoint; a future #1149 follow-up will add `with_backend(...)` here
    /// and flip `backend_bound`.
    pub fn new() -> Self {
        Self {
            backend_bound: false,
        }
    }
}

#[async_trait]
impl Tool for ImageGenerationTool {
    fn name(&self) -> &str {
        "image_generation"
    }

    fn description(&self) -> &str {
        "Generate an image from a text prompt. STUB: no native or skill backend is bound yet (#1149 follow-up); calls return a typed `coding_tool_unsupported` error envelope."
    }

    fn tags(&self) -> &[&str] {
        &["media", "code"]
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Free-form text prompt describing the image to generate"
                },
                "size": {
                    "type": "string",
                    "description": "Optional output size hint (e.g. `1024x1024`). Provider-specific; reserved for the backend-bound variant."
                },
                "n": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 4,
                    "description": "Optional number of images to generate (1-4). Reserved for the backend-bound variant."
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: ImageGenerationInput =
            serde_json::from_value(args.clone()).wrap_err("invalid image_generation input")?;
        let prompt = input
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        if prompt.is_none() {
            return Ok(ToolResult {
                output: "image_generation requires a non-empty `prompt`".to_string(),
                success: false,
                structured_metadata: Some(json!({
                    "codex_tool": "image_generation",
                    "error_kind": "coding_tool_denied",
                    "reason": "missing_prompt",
                })),
                ..Default::default()
            });
        }
        // `backend_bound` is reserved for the #1149 follow-up. Until a real
        // backend is wired, every call returns the typed unsupported
        // envelope; we keep the field on the struct so the future upgrade
        // is a behaviour change, not an API break.
        if self.backend_bound {
            // Unreachable until #1149 follow-up; the stub constructor
            // always sets `backend_bound = false`.
            return Ok(ToolResult {
                output: "image_generation: backend bound but no implementation available"
                    .to_string(),
                success: false,
                structured_metadata: Some(json!({
                    "codex_tool": "image_generation",
                    "error_kind": "coding_tool_missing",
                })),
                ..Default::default()
            });
        }
        let prompt = prompt.unwrap_or("");
        Ok(ToolResult {
            output: json!({
                "error": "image_generation has no native or skill backend bound on this profile",
                "follow_up": "https://github.com/octos-org/octos/issues/1149",
                "prompt": prompt,
            })
            .to_string(),
            success: false,
            structured_metadata: Some(json!({
                "codex_tool": "image_generation",
                "error_kind": "coding_tool_unsupported",
                "reason": "no_backend_bound",
                "follow_up_issue": "https://github.com/octos-org/octos/issues/1149",
                "accepted_input": {
                    "prompt": prompt,
                    "size": input.size,
                    "n": input.n,
                },
            })),
            ..Default::default()
        })
    }
}

/// Tokenise a query into lowercase words, dropping anything shorter than two
/// characters. Used by both `tool_search` (fallback when no exact substring
/// match exists) and `tool_suggest`.
fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.len() >= 2)
        .map(|token| token.to_lowercase())
        .collect()
}

fn search_catalog<'a>(
    catalog: &'a [ToolCatalogEntry],
    query: &str,
    limit: usize,
) -> Vec<&'a ToolCatalogEntry> {
    if query.is_empty() {
        return catalog.iter().take(limit).collect();
    }
    let tokens = tokenize_query(query);
    let mut scored: Vec<(&ToolCatalogEntry, i32)> = catalog
        .iter()
        .filter_map(|entry| {
            let score = catalog_score(entry, query, &tokens);
            if score > 0 {
                Some((entry, score))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    scored.into_iter().take(limit).map(|(e, _)| e).collect()
}

fn suggest_catalog<'a>(
    catalog: &'a [ToolCatalogEntry],
    task: &str,
    limit: usize,
) -> Vec<(&'a ToolCatalogEntry, i32)> {
    if task.is_empty() {
        return catalog.iter().take(limit).map(|e| (e, 0)).collect();
    }
    let tokens = tokenize_query(task);
    let mut scored: Vec<(&ToolCatalogEntry, i32)> = catalog
        .iter()
        .map(|entry| (entry, catalog_score(entry, task, &tokens)))
        .filter(|(_, score)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    scored.into_iter().take(limit).collect()
}

fn catalog_score(entry: &ToolCatalogEntry, query: &str, tokens: &[String]) -> i32 {
    let name = entry.name.to_lowercase();
    let description = entry.description.to_lowercase();
    let tags: Vec<String> = entry.tags.iter().map(|t| t.to_lowercase()).collect();
    let mut score = 0_i32;
    // Exact-name and prefix matches dominate so e.g. `tool_search query="patch"`
    // lands on `apply_patch` ahead of any tool whose description merely mentions
    // patching.
    if !query.is_empty() {
        if name == query {
            score += 100;
        } else if name.contains(query) {
            score += 50;
        }
        if description.contains(query) {
            score += 10;
        }
    }
    for token in tokens {
        if name.contains(token) {
            score += 8;
        }
        if description.contains(token) {
            score += 3;
        }
        if tags.iter().any(|tag| tag.contains(token)) {
            score += 4;
        }
    }
    score
}

#[cfg(test)]
#[path = "coding_tools_tests.rs"]
mod tests;
