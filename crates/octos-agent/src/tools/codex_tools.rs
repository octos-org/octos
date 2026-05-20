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
        if guard.len() > MAX_CAPTURE_BYTES * 2 {
            let keep_from = guard.len().saturating_sub(MAX_CAPTURE_BYTES);
            let trimmed = guard[keep_from..].to_string();
            *guard = format!("... (earlier output truncated)\n{trimmed}");
        }
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

#[derive(Debug, Deserialize)]
struct ApplyPatchInput {
    #[serde(default)]
    patch: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    diff: Option<String>,
}

pub struct ApplyPatchTool {
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    file_access: FileAccessMode,
}

impl ApplyPatchTool {
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

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a Codex-style patch. Supports Add File, Delete File, and exact-match Update File hunks; also accepts {path,diff} for unified diffs."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Codex apply_patch envelope beginning with *** Begin Patch"
                },
                "path": {
                    "type": "string",
                    "description": "Single file path when applying a unified diff"
                },
                "diff": {
                    "type": "string",
                    "description": "Unified diff for path"
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let input: ApplyPatchInput =
            serde_json::from_value(args.clone()).wrap_err("invalid apply_patch input")?;
        if !self.file_access.allows_write() {
            return Ok(ToolResult {
                output: "apply_patch is not permitted by read-only filesystem access".to_string(),
                success: false,
                ..Default::default()
            });
        }
        if let (Some(path), Some(diff)) = (input.path.as_deref(), input.diff.as_deref()) {
            return self.apply_unified_diff(ctx, path, diff).await;
        }
        let Some(patch) = input.patch.as_deref() else {
            return Ok(ToolResult {
                output: "apply_patch requires either patch or {path,diff}".to_string(),
                success: false,
                ..Default::default()
            });
        };
        self.apply_codex_patch(ctx, patch).await
    }
}

impl ApplyPatchTool {
    async fn apply_unified_diff(
        &self,
        ctx: &ToolContext,
        path: &str,
        diff: &str,
    ) -> Result<ToolResult> {
        let tool = super::DiffEditTool::new(&self.base_dir)
            .with_filesystem_scope(self.filesystem_scope)
            .with_file_access(self.file_access);
        tool.execute_with_context(ctx, &json!({ "path": path, "diff": diff }))
            .await
    }

    async fn apply_codex_patch(&self, ctx: &ToolContext, patch: &str) -> Result<ToolResult> {
        let sections = parse_codex_patch(patch);
        if sections.is_empty() {
            return Ok(ToolResult {
                output: "No apply_patch sections found".to_string(),
                success: false,
                ..Default::default()
            });
        }

        let mut modified = Vec::new();
        // #972 / M14-B — capture a per-section operation summary so the
        // AppUI diff preview flow can render an "applied X to Y files"
        // card without round-tripping back through `read_file` to figure
        // out which paths changed. Each entry mirrors the parsed patch
        // section: { op: add|update|delete, path }.
        let mut diff_preview = Vec::new();
        for section in sections {
            let path = match super::resolve_path_with_scope(
                &self.base_dir,
                &section.path,
                self.filesystem_scope,
            ) {
                Ok(path) => path,
                Err(_) => {
                    return Ok(ToolResult {
                        output: format!("Path outside working directory: {}", section.path),
                        success: false,
                        ..Default::default()
                    });
                }
            };
            let op_label = match section.kind {
                PatchSectionKind::Add => {
                    let content = section
                        .lines
                        .iter()
                        .filter_map(|line| line.strip_prefix('+'))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    super::write_no_follow(&path, content.as_bytes()).await?;
                    "add"
                }
                PatchSectionKind::Delete => {
                    tokio::fs::remove_file(&path).await?;
                    "delete"
                }
                PatchSectionKind::Update => {
                    let content = super::read_no_follow(&path).await?;
                    let updated = apply_exact_update_hunks(&content, &section.lines)?;
                    super::write_no_follow(&path, updated.as_bytes()).await?;
                    "update"
                }
            };
            if let Some(cache) = ctx.file_state_cache.as_ref() {
                cache.invalidate(&path);
            }
            diff_preview.push(json!({ "op": op_label, "path": section.path.clone() }));
            modified.push(section.path);
        }

        Ok(ToolResult {
            output: format!("Applied patch to {}", modified.join(", ")),
            success: true,
            file_modified: modified.first().map(|path| self.base_dir.join(path)),
            // #972 / M14-B — structured diff preview event consumed by the
            // AppUI diff flow. `codex_tool = "apply_patch"` matches the
            // model-visible tool name so the client routing stays uniform
            // with `update_plan` / `request_user_input`.
            structured_metadata: Some(json!({
                "codex_tool": "apply_patch",
                "diff_preview": diff_preview,
                "modified_paths": modified,
            })),
            ..Default::default()
        })
    }
}

#[derive(Clone, Copy)]
enum PatchSectionKind {
    Add,
    Delete,
    Update,
}

struct PatchSection {
    kind: PatchSectionKind,
    path: String,
    lines: Vec<String>,
}

fn parse_codex_patch(patch: &str) -> Vec<PatchSection> {
    let mut sections = Vec::new();
    let mut current: Option<PatchSection> = None;
    for raw in patch.lines() {
        if let Some(path) = raw.strip_prefix("*** Add File: ") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            current = Some(PatchSection {
                kind: PatchSectionKind::Add,
                path: path.trim().to_string(),
                lines: Vec::new(),
            });
        } else if let Some(path) = raw.strip_prefix("*** Delete File: ") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            current = Some(PatchSection {
                kind: PatchSectionKind::Delete,
                path: path.trim().to_string(),
                lines: Vec::new(),
            });
        } else if let Some(path) = raw.strip_prefix("*** Update File: ") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            current = Some(PatchSection {
                kind: PatchSectionKind::Update,
                path: path.trim().to_string(),
                lines: Vec::new(),
            });
        } else if raw.starts_with("*** End Patch") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
        } else if let Some(section) = current.as_mut() {
            section.lines.push(raw.to_string());
        }
    }
    if let Some(section) = current {
        sections.push(section);
    }
    sections
}

fn apply_exact_update_hunks(content: &str, patch_lines: &[String]) -> Result<String> {
    let had_trailing_newline = content.ends_with('\n');
    let mut file_lines: Vec<String> = content.lines().map(ToOwned::to_owned).collect();
    let mut index = 0;
    while index < patch_lines.len() {
        while index < patch_lines.len()
            && (patch_lines[index].starts_with("@@") || patch_lines[index].is_empty())
        {
            index += 1;
        }
        let mut old = Vec::new();
        let mut new = Vec::new();
        while index < patch_lines.len() && !patch_lines[index].starts_with("@@") {
            let line = &patch_lines[index];
            if let Some(rest) = line.strip_prefix('-') {
                old.push(rest.to_string());
            } else if let Some(rest) = line.strip_prefix('+') {
                new.push(rest.to_string());
            } else if let Some(rest) = line.strip_prefix(' ') {
                old.push(rest.to_string());
                new.push(rest.to_string());
            }
            index += 1;
        }
        if old.is_empty() && new.is_empty() {
            continue;
        }
        let Some(pos) = find_line_block(&file_lines, &old) else {
            eyre::bail!("apply_patch update hunk did not match file content");
        };
        file_lines.splice(pos..pos + old.len(), new);
    }
    let mut output = file_lines.join("\n");
    if had_trailing_newline {
        output.push('\n');
    }
    Ok(output)
}

fn find_line_block(haystack: &[String], needle: &[String]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

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
        }
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
        let mut cmd = self.sandbox.wrap_command(&command, &cwd);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
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
        let result = timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;
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
                let max = input.max_output_tokens.unwrap_or(MAX_CAPTURE_BYTES);
                Ok(ToolResult {
                    output: truncate_output(text, max),
                    success: output.status.success(),
                    ..Default::default()
                })
            }
            Ok(Err(error)) => Ok(ToolResult {
                output: format!("Failed to execute command: {error}"),
                success: false,
                ..Default::default()
            }),
            Err(_) => Ok(ToolResult {
                output: format!("Command timed out after {timeout_secs} seconds"),
                success: false,
                ..Default::default()
            }),
        }
    }

    async fn spawn_session(
        &self,
        command: String,
        cwd: PathBuf,
        input: ExecCommandInput,
    ) -> Result<ToolResult> {
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
        if let Some(stdout) = stdout {
            tokio::spawn(append_reader_output(stdout, output.clone(), "stdout"));
        }
        if let Some(stderr) = stderr {
            tokio::spawn(append_reader_output(stderr, output.clone(), "stderr"));
        }
        let exit_code_for_wait = exit_code.clone();
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|status| status.code());
            *exit_code_for_wait.lock().await = Some(code.unwrap_or(-1));
        });
        exec_sessions().lock().await.insert(
            session_id.clone(),
            ExecSession {
                stdin: Arc::new(Mutex::new(stdin)),
                output: output.clone(),
                exit_code: exit_code.clone(),
            },
        );
        tokio::time::sleep(Duration::from_millis(
            input.yield_time_ms.unwrap_or(DEFAULT_EXEC_YIELD_MS),
        ))
        .await;
        let captured = output.lock().await.clone();
        let code = *exit_code.lock().await;
        Ok(ToolResult {
            output: json!({
                "session_id": session_id,
                "running": code.is_none(),
                "exit_code": code,
                "output": truncate_output(captured, input.max_output_tokens.unwrap_or(MAX_CAPTURE_BYTES)),
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
        let output = session.output.lock().await.clone();
        let code = *session.exit_code.lock().await;
        Ok(ToolResult {
            output: json!({
                "session_id": input.session_id,
                "running": code.is_none(),
                "exit_code": code,
                "output": truncate_output(output, input.max_output_tokens.unwrap_or(MAX_CAPTURE_BYTES)),
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

async fn update_plan_body(_: &dyn Tool, args: &Value, _: &ToolContext) -> Result<ToolResult> {
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
            "message": "User input request recorded; no synchronous host response channel is attached to this runtime."
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

fn normalize_spawn_agent_args(args: &Value) -> Value {
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
    if !out.contains_key("model") {
        if let Some(model) = args.get("model").and_then(Value::as_str) {
            out.insert("model".to_string(), Value::String(model.to_string()));
        }
    }

    let mut extra = out
        .get("additional_instructions")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
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
        json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"},
                "items": {"type": "array"},
                "agent_type": {"type": "string"},
                "fork_context": {"type": "boolean"},
                "model": {"type": "string"},
                "reasoning_effort": {"type": "string"},
                "task": {"type": "string"},
                "label": {"type": "string"},
                "mode": {"type": "string", "enum": ["background", "sync"]},
                "allowed_tools": {"type": "array", "items": {"type": "string"}}
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let Some(delegate) = self.delegate.as_ref() else {
            return spawn_agent_without_delegate(args, ctx).await;
        };
        let spawn_args = normalize_spawn_agent_args(args);
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
        if let Some(task) = task {
            payload["agent_id"] = json!(task.id);
            payload["status"] = json!(task.status.as_str());
            payload["runtime_state"] = json!(format!("{:?}", task.runtime_state));
            payload["child_session_key"] = json!(task.child_session_key);
            payload["terminal"] = json!(task.status.is_terminal());
        }
        Ok(ToolResult {
            output: payload.to_string(),
            success: true,
            structured_metadata: Some(json!({
                "codex_tool": "spawn_agent",
                "octos_tool": "spawn",
                "spawn_args": spawn_args,
            })),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    const CODEX_P0: &[&str] = &[
        "apply_patch",
        "exec_command",
        "write_stdin",
        "update_plan",
        "request_user_input",
        "spawn_agent",
        "send_input",
        "resume_agent",
        "wait_agent",
        "close_agent",
    ];

    #[test]
    fn builtins_expose_codex_p0_tool_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = ToolRegistry::with_builtins(temp.path());
        let names: std::collections::HashSet<_> =
            registry.specs().into_iter().map(|spec| spec.name).collect();
        for name in CODEX_P0 {
            assert!(names.contains(*name), "{name} should be model-visible");
        }
    }

    /// #972 / M14-B acceptance: `apply_patch` MUST produce a diff
    /// preview compatible with the AppUI diff flow. The contract is a
    /// `structured_metadata` envelope with `codex_tool = "apply_patch"`,
    /// a `diff_preview` array of `{ op, path }` entries (one per parsed
    /// patch section), and a flat `modified_paths` list the diff panel
    /// can render without re-parsing the patch envelope.
    #[tokio::test]
    async fn apply_patch_emits_diff_preview_structured_metadata() {
        use std::path::PathBuf;
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace: PathBuf = temp.path().to_path_buf();
        let tool = ApplyPatchTool::new(workspace.clone());
        // Add a fresh file via the Codex patch envelope.
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Add File: hello.txt\n",
            "+hello\n",
            "+world\n",
            "*** End Patch\n",
        );
        let result = tool
            .execute(&json!({ "patch": patch }))
            .await
            .expect("apply_patch ok");
        assert!(result.success, "apply_patch must succeed on Add File");
        let meta = result
            .structured_metadata
            .as_ref()
            .expect("apply_patch must emit structured_metadata");
        assert_eq!(meta["codex_tool"], json!("apply_patch"));
        assert_eq!(meta["modified_paths"], json!(["hello.txt"]));
        let preview = meta["diff_preview"]
            .as_array()
            .expect("diff_preview must be an array");
        assert_eq!(preview.len(), 1);
        assert_eq!(preview[0]["op"], json!("add"));
        assert_eq!(preview[0]["path"], json!("hello.txt"));
        // Sanity: the file we asked for actually exists with the
        // intended content (this also guards against the metadata
        // claim drifting from the underlying write).
        let contents = std::fs::read_to_string(workspace.join("hello.txt"))
            .expect("created file must be readable");
        assert!(contents.contains("hello"));
        assert!(contents.contains("world"));
    }

    /// #972 / M14-B acceptance: `update_plan` MUST generate a structured
    /// UI event so the AppUI layer can render the plan card without
    /// parsing free-form `output` text. The contract is the
    /// `structured_metadata` envelope with `codex_tool = "update_plan"`
    /// and the model-provided plan echoed under `plan`.
    #[tokio::test]
    async fn update_plan_emits_structured_metadata_event() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = ToolRegistry::with_builtins(temp.path());
        let tool = registry
            .get("update_plan")
            .expect("update_plan tool registered");
        let plan_args = json!({
            "plan": [
                { "id": "p1", "title": "scaffold", "status": "in_progress" },
                { "id": "p2", "title": "tests", "status": "pending" }
            ]
        });
        let result = tool.execute(&plan_args).await.expect("update_plan ok");
        assert!(result.success, "update_plan must succeed");
        let meta = result
            .structured_metadata
            .as_ref()
            .expect("update_plan must emit structured_metadata");
        assert_eq!(meta["codex_tool"], json!("update_plan"));
        assert_eq!(
            meta["plan"], plan_args,
            "echoed plan must match the model-provided arguments"
        );
    }

    /// #972 / M14-B acceptance: `request_user_input` MUST generate a
    /// structured UI event so the AppUI layer can render the user-input
    /// request without parsing the `output` blob. The contract is the
    /// `structured_metadata` envelope with `codex_tool = "request_user_input"`,
    /// the original request echoed under `request`, and a `host_response_channel`
    /// hint that lets the client tell whether a synchronous response path
    /// is wired (M14-E live soak scope) or not (current state).
    #[tokio::test]
    async fn request_user_input_emits_structured_metadata_event() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = ToolRegistry::with_builtins(temp.path());
        let tool = registry
            .get("request_user_input")
            .expect("request_user_input tool registered");
        let request_args = json!({
            "prompt": "Pick a deploy target",
            "choices": ["staging", "prod"],
        });
        let result = tool
            .execute(&request_args)
            .await
            .expect("request_user_input ok");
        assert!(result.success);
        let meta = result
            .structured_metadata
            .as_ref()
            .expect("request_user_input must emit structured_metadata");
        assert_eq!(meta["codex_tool"], json!("request_user_input"));
        assert_eq!(
            meta["request"], request_args,
            "request payload must round-trip into the structured event"
        );
        assert!(
            meta.get("host_response_channel").is_some(),
            "structured event must declare host response channel state for the client"
        );
    }

    struct FakeSpawnTool;

    #[async_trait::async_trait]
    impl Tool for FakeSpawnTool {
        fn name(&self) -> &str {
            "spawn"
        }

        fn description(&self) -> &str {
            "fake spawn"
        }

        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, args: &Value) -> Result<ToolResult> {
            self.execute_with_context(&ToolContext::zero(), args).await
        }

        async fn execute_with_context(
            &self,
            ctx: &ToolContext,
            args: &Value,
        ) -> Result<ToolResult> {
            let supervisor = ctx.task_supervisor.as_ref().expect("supervisor");
            let task_id = supervisor.register_with_input(
                "spawn",
                "fake-call",
                ctx.parent_session_key.as_deref(),
                Some(args.clone()),
            );
            supervisor.mark_running(&task_id);
            Ok(ToolResult {
                output: "spawned fake worker".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn spawn_agent_delegates_to_registered_spawn_tool() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut registry = ToolRegistry::with_builtins(temp.path());
        registry.register(FakeSpawnTool);
        let supervisor = registry.supervisor();
        let ctx = ToolContext {
            task_supervisor: Some(supervisor.clone()),
            parent_session_key: Some("api:test".to_string()),
            ..ToolContext::zero()
        };
        let result = registry
            .execute_with_context(
                &ctx,
                "spawn_agent",
                &json!({
                    "message": "inspect parity",
                    "agent_type": "worker",
                    "reasoning_effort": "high"
                }),
            )
            .await
            .expect("spawn_agent");
        assert!(result.success, "{}", result.output);
        let payload: Value = serde_json::from_str(&result.output).expect("json payload");
        let agent_id = payload["agent_id"].as_str().expect("agent id");
        let task = supervisor.get_task(agent_id).expect("task registered");
        let input = task.tool_input.expect("tool input");
        assert_eq!(input["task"], "inspect parity");
        assert_eq!(input["label"], "codex-worker");
        assert!(
            input["additional_instructions"]
                .as_str()
                .unwrap()
                .contains("reasoning_effort: high")
        );
    }

    #[tokio::test]
    async fn apply_patch_adds_and_updates_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ApplyPatchTool::new(temp.path());
        let add = tool
            .execute(&json!({
                "patch": "*** Begin Patch\n*** Add File: demo.txt\n+hello\n+world\n*** End Patch\n"
            }))
            .await
            .expect("apply add");
        assert!(add.success, "{}", add.output);
        assert_eq!(
            tokio::fs::read_to_string(temp.path().join("demo.txt"))
                .await
                .expect("read added"),
            "hello\nworld"
        );

        let update = tool
            .execute(&json!({
                "patch": "*** Begin Patch\n*** Update File: demo.txt\n@@\n hello\n-world\n+codex\n*** End Patch\n"
            }))
            .await
            .expect("apply update");
        assert!(update.success, "{}", update.output);
        assert_eq!(
            tokio::fs::read_to_string(temp.path().join("demo.txt"))
                .await
                .expect("read updated"),
            "hello\ncodex"
        );
    }

    #[tokio::test]
    async fn exec_command_runs_to_completion() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = ToolRegistry::with_builtins(temp.path());
        let result = registry
            .execute("exec_command", &json!({"cmd": "printf codex"}))
            .await
            .expect("exec command");
        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("codex"));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn write_stdin_talks_to_exec_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = ToolRegistry::with_builtins(temp.path());
        let started = registry
            .execute(
                "exec_command",
                &json!({
                    "cmd": "read line; echo got:$line",
                    "tty": true,
                    "yield_time_ms": 20
                }),
            )
            .await
            .expect("start exec session");
        assert!(started.success, "{}", started.output);
        let payload: Value = serde_json::from_str(&started.output).expect("session payload");
        let session_id = payload["session_id"].as_str().expect("session_id");
        let written = registry
            .execute(
                "write_stdin",
                &json!({
                    "session_id": session_id,
                    "chars": "octos\n",
                    "yield_time_ms": 100
                }),
            )
            .await
            .expect("write stdin");
        assert!(written.success, "{}", written.output);
        assert!(written.output.contains("got:octos"), "{}", written.output);
    }
}
