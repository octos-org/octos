//! Shell tool for executing commands.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tokio::time::timeout;

use super::{
    ConcurrencyClass, TOOL_APPROVAL_CTX, TOOL_CTX, Tool, ToolApprovalDecision, ToolApprovalRequest,
    ToolContext, ToolResult,
};
use crate::policy::{ApprovalPolicy, CommandPolicy, Decision, SafePolicy};
use crate::sandbox::{NoSandbox, Sandbox};
use crate::subprocess_env::{EnvAllowlist, sanitize_command_env};

/// Tool for executing shell commands.
pub struct ShellTool {
    /// Timeout for command execution.
    timeout: Duration,
    /// Working directory for commands.
    cwd: std::path::PathBuf,
    /// Policy for command approval.
    policy: Arc<dyn CommandPolicy>,
    /// Runtime approval behavior for commands that request approval.
    approval_policy: ApprovalPolicy,
    /// Sandbox for command isolation.
    sandbox: Arc<dyn Sandbox>,
}

impl ShellTool {
    /// Create a new shell tool with safe defaults.
    pub fn new(cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            timeout: Duration::from_secs(120),
            cwd: cwd.into(),
            policy: Arc::new(SafePolicy::default()),
            approval_policy: ApprovalPolicy::Ask,
            sandbox: Arc::new(NoSandbox),
        }
    }

    /// Set the timeout for commands.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set a custom command policy.
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

#[cfg(windows)]
const NULL_DEVICE_PATH: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE_PATH: &str = "/dev/null";

fn contains_git_invocation(command: &str) -> bool {
    command
        .split(['\n', ';'])
        .flat_map(|segment| segment.split("&&"))
        .flat_map(|segment| segment.split("||"))
        .any(segment_invokes_git)
}

fn segment_invokes_git(segment: &str) -> bool {
    let mut remaining = segment.trim_start();
    loop {
        if remaining == "git" || remaining.starts_with("git ") {
            return true;
        }
        let Some(token_end) = remaining.find(char::is_whitespace) else {
            return false;
        };
        let token = &remaining[..token_end];
        if token == "env" || looks_like_env_assignment(token) {
            remaining = remaining[token_end..].trim_start();
            continue;
        }
        return false;
    }
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
struct ShellInput {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
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
        // shell serializes the whole batch. See M8.8.
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
            serde_json::from_value(args.clone()).wrap_err("invalid shell tool input")?;

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

        // Clamp timeout to [1, 600] seconds to prevent abuse
        const MIN_TIMEOUT: u64 = 1;
        const MAX_TIMEOUT: u64 = 600;
        let timeout_duration = input
            .timeout_secs
            .map(|s| Duration::from_secs(s.clamp(MIN_TIMEOUT, MAX_TIMEOUT)))
            .unwrap_or(self.timeout);

        // Execute command (through sandbox).
        // Spawn the child, grab its PID, then timeout on wait_with_output().
        // If timeout fires, kill by PID to prevent orphaned processes.
        // (wait_with_output() takes ownership of child, so we save the PID first.)
        let mut cmd = self.sandbox.wrap_command(&input.command, effective_cwd);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        apply_frontend_tool_env(&mut cmd, effective_cwd);
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

                    // 1. Send SIGTERM to process group for graceful shutdown
                    let _ = StdCommand::new("kill")
                        .args(["-15", &format!("-{pid}")])
                        .status();
                    let _ = StdCommand::new("kill")
                        .args(["-15", &pid.to_string()])
                        .status();

                    // 2. Brief grace period, then SIGKILL only if still alive.
                    // Check /proc/{pid} (Linux) or kill -0 (portable) to avoid
                    // killing a recycled PID.
                    tokio::time::sleep(Duration::from_millis(500)).await;

                    let still_alive = StdCommand::new("kill")
                        .args(["-0", &pid.to_string()])
                        .status()
                        .is_ok_and(|s| s.success());

                    if still_alive {
                        let _ = StdCommand::new("kill")
                            .args(["-9", &format!("-{pid}")])
                            .status();
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

    #[tokio::test]
    async fn test_shell_sets_frontend_build_env() {
        let cwd = std::env::temp_dir().join(format!("octos-shell-env-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();

        let tool = ShellTool::new(&cwd);
        let result = tool
            .execute(&serde_json::json!({
                "command": "printf '%s\\n%s\\n' \"$ASTRO_TELEMETRY_DISABLED\" \"$NPM_CONFIG_CACHE\""
            }))
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
        assert!(!contains_git_invocation("printf 'git diff -- notes.txt'"));
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
                .contains(&canonical_workspace.to_string_lossy().to_string()),
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
                .contains(&canonical_hinted.to_string_lossy().to_string()),
            "expected hinted workspace ({}) in shell output, got: {}",
            canonical_hinted.display(),
            result.output
        );
        assert!(
            !result
                .output
                .contains(&canonical_default_scope.to_string_lossy().to_string()),
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
        assert!(
            result
                .output
                .contains(&canonical_legacy.to_string_lossy().to_string()),
            "expected legacy cwd ({}) in shell output, got: {}",
            canonical_legacy.display(),
            result.output
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
