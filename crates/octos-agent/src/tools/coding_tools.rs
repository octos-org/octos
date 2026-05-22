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

    // -----------------------------------------------------------------------
    // #972 / M14-B P1 tests — `view_image`, `tool_search`, `tool_suggest`.
    // -----------------------------------------------------------------------

    /// 8-byte PNG header (the only part the format detector cares about) plus
    /// a zero-IHDR-length marker; enough to make `view_image` happy without
    /// pulling in the `image` crate.
    const PNG_MAGIC: [u8; 12] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];

    #[tokio::test]
    async fn view_image_reports_format_and_size_for_png() {
        let temp = tempfile::tempdir().expect("tempdir");
        let png = temp.path().join("logo.png");
        std::fs::write(&png, PNG_MAGIC).expect("write png");
        let tool = ViewImageTool::new(temp.path());
        let result = tool
            .execute(&json!({ "path": "logo.png" }))
            .await
            .expect("view_image ok");
        assert!(result.success, "{}", result.output);
        let payload: Value = serde_json::from_str(&result.output).expect("json payload");
        assert_eq!(payload["format"], json!("png"));
        assert_eq!(payload["mime_type"], json!("image/png"));
        assert_eq!(payload["byte_length"], json!(PNG_MAGIC.len()));
        let meta = result.structured_metadata.expect("structured metadata");
        assert_eq!(meta["codex_tool"], json!("view_image"));
        assert_eq!(meta["format"], json!("png"));
    }

    /// Codex review #1153 P2 regression: `FilesystemScope::Host` (granted via
    /// `DangerFullAccess`) lets `view_image` read images outside the
    /// workspace. Pre-fix, the helper passed `self.base_dir` as the
    /// ancestor-walk stop unconditionally. For a host path like `/tmp/foo.png`
    /// on macOS, the walk never reached the workspace and refused `/tmp`
    /// (which is a symlink to `/private/tmp` on macOS). Now host-scope skips
    /// the ancestor walk entirely; the Unix O_NOFOLLOW leaf guard still
    /// protects the final-component symlink case.
    #[tokio::test]
    async fn view_image_host_scope_accepts_path_outside_workspace_per_1153() {
        // Build a host path under a SECOND tempdir so it's outside `base_dir`.
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let png = outside.path().join("host.png");
        std::fs::write(&png, PNG_MAGIC).expect("write png");

        let tool =
            ViewImageTool::new(workspace.path()).with_filesystem_scope(FilesystemScope::Host);

        // Absolute path: host scope must accept it even though it lives
        // outside `workspace.path()`.
        let result = tool
            .execute(&json!({ "path": png.to_string_lossy() }))
            .await
            .expect("view_image runs");

        assert!(
            result.success,
            "host-scope view_image must accept paths outside the workspace; got error: {}",
            result.output
        );
        let payload: Value = serde_json::from_str(&result.output).expect("json payload");
        assert_eq!(payload["format"], json!("png"));
    }

    /// Codex review #1153 P2 rev2: when host-scope skips the
    /// ancestor walk, the Windows leaf-symlink guard goes with it
    /// (Unix still has O_NOFOLLOW, but Windows has no replacement).
    /// The new `reject_leaf_symlink` must catch a leaf symlink even
    /// in host scope. This test exercises the Unix path; the same
    /// guard runs on Windows where it's the ONLY leaf no-follow check.
    #[cfg(unix)]
    #[tokio::test]
    async fn view_image_host_scope_still_rejects_leaf_symlink_per_1153() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let target = outside.path().join("real.png");
        std::fs::write(&target, PNG_MAGIC).expect("write real png");
        let link = outside.path().join("link.png");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let tool =
            ViewImageTool::new(workspace.path()).with_filesystem_scope(FilesystemScope::Host);

        let result = tool
            .execute(&json!({ "path": link.to_string_lossy() }))
            .await
            .expect("view_image runs");

        assert!(
            !result.success,
            "host-scope view_image must still reject a leaf symlink even when ancestor walk is skipped; got: {}",
            result.output,
        );
    }

    #[tokio::test]
    async fn view_image_fails_when_path_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ViewImageTool::new(temp.path());
        let result = tool
            .execute(&json!({ "path": "absent.png" }))
            .await
            .expect("view_image runs");
        assert!(!result.success);
        let meta = result.structured_metadata.expect("structured metadata");
        assert_eq!(meta["codex_tool"], json!("view_image"));
        assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
    }

    #[tokio::test]
    async fn view_image_rejects_non_image_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let txt = temp.path().join("notes.txt");
        std::fs::write(&txt, b"hello, not an image").expect("write text");
        let tool = ViewImageTool::new(temp.path());
        let result = tool
            .execute(&json!({ "path": "notes.txt" }))
            .await
            .expect("view_image runs");
        assert!(!result.success);
        let meta = result.structured_metadata.expect("structured metadata");
        assert_eq!(meta["reason"], json!("unrecognised_image_format"));
    }

    /// #1148 codex P2 acceptance: view_image MUST refuse to follow
    /// symlinks (Unix O_NOFOLLOW) so a malicious repo can't trick
    /// the tool into reading a file outside the workspace via a
    /// symlinked image entry.
    #[cfg(unix)]
    #[tokio::test]
    async fn view_image_rejects_symlinked_target() {
        const PNG_MAGIC: [u8; 12] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("real_image.png");
        std::fs::write(&target, PNG_MAGIC).expect("write png");
        let symlink = temp.path().join("link.png");
        std::os::unix::fs::symlink(&target, &symlink).expect("symlink");

        let tool = ViewImageTool::new(temp.path());
        let result = tool
            .execute(&json!({ "path": "link.png" }))
            .await
            .expect("view_image runs");
        assert!(
            !result.success,
            "view_image must reject symlinked targets (O_NOFOLLOW); got success result"
        );
        let meta = result.structured_metadata.expect("structured metadata");
        assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
    }

    /// #1151 acceptance: view_image MUST refuse to traverse a
    /// SYMLINKED PARENT DIRECTORY. The Unix `O_NOFOLLOW` flag only
    /// catches a symlink at the final path component, so without an
    /// ancestor walk a malicious workspace could ship
    /// `workspace/link -> /outside/` and `view_image link/real.png`
    /// would read `/outside/real.png` (outside the workspace).
    #[cfg(unix)]
    #[tokio::test]
    async fn view_image_rejects_parent_symlink_directory() {
        const PNG_MAGIC: [u8; 12] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        let temp = tempfile::tempdir().expect("tempdir");
        // Two sibling directories under the same tempdir: the
        // workspace, and an `outside` directory that contains the
        // real image. The workspace itself contains a symlink
        // `imgs -> outside`. Lexically `workspace/imgs/real.png`
        // looks workspace-relative.
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&workspace).expect("mk workspace");
        std::fs::create_dir(&outside).expect("mk outside");
        std::fs::write(outside.join("real.png"), PNG_MAGIC).expect("write png");
        std::os::unix::fs::symlink(&outside, workspace.join("imgs"))
            .expect("symlink parent directory");

        let tool = ViewImageTool::new(&workspace);
        let result = tool
            .execute(&json!({ "path": "imgs/real.png" }))
            .await
            .expect("view_image runs");
        assert!(
            !result.success,
            "view_image must refuse a SYMLINKED PARENT directory; got success result: {}",
            result.output
        );
        let meta = result.structured_metadata.expect("structured metadata");
        assert_eq!(meta["codex_tool"], json!("view_image"));
        assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
    }

    /// #1151 acceptance: Windows must perform the symlink rejection
    /// BEFORE the open call. Prior to the fix the helper opened the
    /// file first and then called `file.metadata().is_symlink()` —
    /// but `OpenOptions::open` had already followed the symlink, so
    /// the check was silently a no-op. The pre-open `symlink_metadata`
    /// ancestor walk catches the leaf reliably.
    ///
    /// NB: Windows symlink creation requires Developer Mode or admin
    /// privileges. The test silently passes when neither is available
    /// — there is nothing the test can do about an unprivileged CI
    /// runner. The Unix counterpart above gives functional coverage;
    /// this test guards against the platform-specific regression
    /// only when the host can actually create a symlink.
    #[cfg(windows)]
    #[tokio::test]
    async fn view_image_rejects_leaf_symlink_pre_open_on_windows() {
        const PNG_MAGIC: [u8; 12] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("real_image.png");
        std::fs::write(&target, PNG_MAGIC).expect("write png");
        let symlink = temp.path().join("link.png");
        if std::os::windows::fs::symlink_file(&target, &symlink).is_err() {
            // Unprivileged runner — symlinks unavailable. Skip
            // rather than fail; the Unix test exercises the same
            // ancestor-walk code path.
            eprintln!(
                "skipping view_image_rejects_leaf_symlink_pre_open_on_windows: symlink_file failed (Developer Mode or admin required)"
            );
            return;
        }

        let tool = ViewImageTool::new(temp.path());
        let result = tool
            .execute(&json!({ "path": "link.png" }))
            .await
            .expect("view_image runs");
        assert!(
            !result.success,
            "view_image must reject a leaf symlink PRE-OPEN on Windows; got success result: {}",
            result.output
        );
        let meta = result.structured_metadata.expect("structured metadata");
        assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
    }

    /// #1148 codex P2 acceptance: view_image must read only a bounded
    /// header — it should NOT allocate the entire file for magic-byte
    /// sniffing. The PNG test above only writes 12 bytes; this one
    /// writes a 10MB file but still gets a correct format report
    /// with proper byte_length, proving we read only the header.
    #[tokio::test]
    async fn view_image_reads_only_bounded_header_for_large_file() {
        const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("big.png");
        let mut bytes = Vec::with_capacity(10_000_000);
        bytes.extend_from_slice(&PNG_MAGIC);
        bytes.resize(10_000_000, 0u8);
        std::fs::write(&path, &bytes).expect("write big png");

        let tool = ViewImageTool::new(temp.path());
        let result = tool
            .execute(&json!({ "path": "big.png" }))
            .await
            .expect("view_image runs");
        assert!(result.success, "10MB image with valid header must succeed");
        let payload: Value = serde_json::from_str(&result.output).expect("payload");
        assert_eq!(payload["format"], json!("png"));
        assert_eq!(payload["byte_length"], json!(10_000_000));
    }

    fn sample_catalog_cell() -> Arc<std::sync::Mutex<Vec<ToolCatalogEntry>>> {
        Arc::new(std::sync::Mutex::new(sample_catalog()))
    }

    fn sample_catalog() -> Vec<ToolCatalogEntry> {
        vec![
            ToolCatalogEntry::new(
                "apply_patch",
                "Apply a Codex-style patch to files in the workspace",
                vec!["fs".to_string(), "code".to_string()],
            ),
            ToolCatalogEntry::new(
                "exec_command",
                "Run a shell command. Supports long-running sessions.",
                vec!["runtime".to_string(), "code".to_string()],
            ),
            ToolCatalogEntry::new(
                "update_plan",
                "Update the visible task plan",
                vec!["code".to_string()],
            ),
            ToolCatalogEntry::new(
                "web_search",
                "Search the web for an arbitrary query",
                vec!["search".to_string(), "web".to_string()],
            ),
        ]
    }

    #[tokio::test]
    async fn tool_search_returns_matching_tools_for_substring() {
        let tool = ToolSearchTool::new(sample_catalog_cell());
        let result = tool
            .execute(&json!({ "query": "patch" }))
            .await
            .expect("tool_search ok");
        assert!(result.success);
        let payload: Value = serde_json::from_str(&result.output).expect("payload");
        let matches = payload["matches"].as_array().expect("matches");
        assert!(!matches.is_empty(), "expected at least one match");
        assert_eq!(matches[0]["name"], json!("apply_patch"));
        let meta = result.structured_metadata.expect("structured metadata");
        assert_eq!(meta["codex_tool"], json!("tool_search"));
    }

    #[tokio::test]
    async fn tool_search_returns_empty_matches_for_unrelated_query() {
        let tool = ToolSearchTool::new(sample_catalog_cell());
        let result = tool
            .execute(&json!({ "query": "zzz_not_a_tool" }))
            .await
            .expect("tool_search ok");
        assert!(result.success);
        let payload: Value = serde_json::from_str(&result.output).expect("payload");
        assert!(payload["matches"].as_array().expect("matches").is_empty());
    }

    #[tokio::test]
    async fn tool_search_honours_limit() {
        let tool = ToolSearchTool::new(sample_catalog_cell());
        let result = tool
            .execute(&json!({ "query": "code", "limit": 2 }))
            .await
            .expect("tool_search ok");
        assert!(result.success);
        let payload: Value = serde_json::from_str(&result.output).expect("payload");
        assert!(payload["matches"].as_array().unwrap().len() <= 2);
    }

    #[tokio::test]
    async fn tool_suggest_ranks_relevant_tools_first() {
        let tool = ToolSuggestTool::new(sample_catalog_cell());
        let result = tool
            .execute(&json!({ "task": "I want to apply a code patch to a file" }))
            .await
            .expect("tool_suggest ok");
        assert!(result.success);
        let payload: Value = serde_json::from_str(&result.output).expect("payload");
        let suggestions = payload["suggestions"].as_array().expect("suggestions");
        assert!(
            !suggestions.is_empty(),
            "expected suggestions for code task"
        );
        assert_eq!(suggestions[0]["name"], json!("apply_patch"));
        // Suggestions for a coding task should not surface `web_search`.
        let names: Vec<&str> = suggestions
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        assert!(
            !names.contains(&"web_search"),
            "web_search should not be suggested for a code-patch task: {names:?}"
        );
    }

    #[tokio::test]
    async fn tool_suggest_accepts_query_alias_for_task() {
        let tool = ToolSuggestTool::new(sample_catalog_cell());
        let result = tool
            .execute(&json!({ "query": "shell command" }))
            .await
            .expect("tool_suggest ok");
        assert!(result.success);
        let payload: Value = serde_json::from_str(&result.output).expect("payload");
        let suggestions = payload["suggestions"].as_array().expect("suggestions");
        assert_eq!(suggestions[0]["name"], json!("exec_command"));
    }

    /// #1148 codex P2 acceptance: tool_search must reflect tools
    /// registered AFTER `with_builtins` (chat/gateway/profile setup
    /// paths inject MCP/plugin/pipeline/memory tools). The discovery
    /// surface should be live, not a frozen snapshot.
    #[tokio::test]
    async fn tool_search_reflects_post_builtins_registrations() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut registry = ToolRegistry::with_builtins(temp.path());

        // Sanity: a freshly-coined tool name doesn't appear pre-registration.
        let search_tool = registry
            .get_tool("tool_search")
            .expect("tool_search registered by with_builtins");
        let result = search_tool
            .execute(&serde_json::json!({ "query": "post_builtin_xyz_unique" }))
            .await
            .expect("tool_search ok");
        let payload: Value = serde_json::from_str(&result.output).expect("payload");
        assert_eq!(
            payload["matches"].as_array().map(Vec::len),
            Some(0),
            "fresh registry should not match unknown tool name yet"
        );

        // Inject a new tool AFTER with_builtins.
        struct PostBuiltinTool;
        #[async_trait::async_trait]
        impl Tool for PostBuiltinTool {
            fn name(&self) -> &str {
                "post_builtin_xyz_unique"
            }
            fn description(&self) -> &str {
                "A tool registered after with_builtins"
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object"})
            }
            async fn execute(&self, _args: &Value) -> eyre::Result<ToolResult> {
                Ok(ToolResult::default())
            }
        }
        registry.register(PostBuiltinTool);

        // Now tool_search MUST find it.
        let result = search_tool
            .execute(&serde_json::json!({ "query": "post_builtin_xyz_unique" }))
            .await
            .expect("tool_search ok");
        let payload: Value = serde_json::from_str(&result.output).expect("payload");
        let matches = payload["matches"].as_array().expect("matches array");
        assert!(
            matches
                .iter()
                .any(|m| m["name"] == json!("post_builtin_xyz_unique")),
            "tool_search must reflect post-builtins registrations (got {:?})",
            matches,
        );
    }

    #[tokio::test]
    async fn builtins_expose_p1_codex_tool_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = ToolRegistry::with_builtins(temp.path());
        let names: std::collections::HashSet<_> =
            registry.specs().into_iter().map(|spec| spec.name).collect();
        for name in &["view_image", "tool_search", "tool_suggest"] {
            assert!(names.contains(*name), "{name} must be model-visible");
        }
    }
}
