//! Shell tool for executing commands.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tokio::time::timeout;

use super::{Tool, ToolResult};
use crate::policy::{CommandPolicy, Decision, SafePolicy};
use crate::sandbox::{NoSandbox, Sandbox};

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
}
