//! Shell tool for executing commands.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tokio::time::timeout;

use super::{ConcurrencyClass, Tool, ToolResult};
use crate::policy::{CommandPolicy, Decision, SafePolicy};
use crate::sandbox::{NoSandbox, Sandbox};
use crate::subprocess_env::{EnvAllowlist, sanitize_command_env};
use crate::tools::TOOL_CTX;

/// Tool for executing shell commands.
pub struct ShellTool {
    /// Timeout for command execution.
    timeout: Duration,
    /// Working directory for commands.
    cwd: std::path::PathBuf,
    /// Policy for command approval.
    policy: Arc<dyn CommandPolicy>,
    /// Sandbox for command isolation.
    sandbox: Box<dyn Sandbox>,
}

impl ShellTool {
    /// Create a new shell tool with safe defaults.
    pub fn new(cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            timeout: Duration::from_secs(120),
            cwd: cwd.into(),
            policy: Arc::new(SafePolicy::default()),
            sandbox: Box::new(NoSandbox),
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

    /// Set a sandbox for command isolation.
    pub fn with_sandbox(mut self, sandbox: Box<dyn Sandbox>) -> Self {
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

fn apply_harness_event_sink_env(cmd: &mut tokio::process::Command) {
    if let Ok(ctx) = TOOL_CTX.try_with(|ctx| ctx.clone()) {
        if let Some(sink) = ctx.harness_event_sink {
            cmd.env("OCTOS_EVENT_SINK", sink);
        }
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
        let input: ShellInput =
            serde_json::from_value(args.clone()).wrap_err("invalid shell tool input")?;

        // Check policy first
        let decision = self.policy.check(&input.command, &self.cwd);
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
                tracing::warn!(command = %input.command, "command requires approval — denied (no interactive approval available)");
                return Ok(ToolResult {
                    output: format!(
                        "Command requires approval and was denied: {}\n\nThis command matches a potentially dangerous pattern (e.g. sudo, rm -rf, git push --force). It cannot be executed without interactive approval.",
                        input.command
                    ),
                    success: false,
                    ..Default::default()
                });
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
        let mut cmd = self.sandbox.wrap_command(&input.command, &self.cwd);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        apply_frontend_tool_env(&mut cmd, &self.cwd);
        apply_git_tool_env(&mut cmd, &input.command);
        sanitize_command_env(&mut cmd, &EnvAllowlist::empty());
        apply_harness_event_sink_env(&mut cmd);

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
}
