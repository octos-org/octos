//! Hook/lifecycle system for running shell commands at agent lifecycle points.
//!
//! Supports 4 events: before/after tool call and before/after LLM call.
//! Before-hooks can deny operations (exit code 1). Circuit breaker auto-disables
//! hooks after consecutive failures.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;

use crate::sandbox::BLOCKED_ENV_VARS;

/// Session-level context injected into hook payloads.
/// Set by the caller (gateway/chat) before the agent loop starts.
#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub session_id: Option<String>,
    pub profile_id: Option<String>,
}

/// Lifecycle events that can trigger hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    BeforeToolCall,
    AfterToolCall,
    BeforeLlmCall,
    AfterLlmCall,
}

/// Configuration for a single hook.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookConfig {
    /// Which lifecycle event triggers this hook.
    pub event: HookEvent,
    /// Command as argv array (no shell interpretation).
    pub command: Vec<String>,
    /// Timeout in milliseconds (default 5000).
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Only trigger for these tool names (tool events only). Empty = all tools.
    #[serde(default)]
    pub tool_filter: Vec<String>,
}

fn default_timeout_ms() -> u64 {
    5000
}

/// Payload sent to hook process as JSON on stdin.
#[derive(Debug, Clone, Serialize)]
pub struct HookPayload {
    pub event: HookEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,

    // Session context (all events)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,

    // Cumulative tracking (after_llm)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_cost: Option<f64>,

    // Provider info (after_llm)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

/// Maximum byte length for arguments/result fields in hook payloads.
const MAX_PAYLOAD_FIELD_BYTES: usize = 1024;

/// Tool names whose arguments and results may contain secrets (file contents,
/// shell output, passwords). Their payloads are replaced with a redaction
/// notice instead of being truncated.
const SENSITIVE_TOOLS: &[&str] = &["shell", "write_file", "read_file"];

/// Truncate a string to at most `max_bytes`, cutting at a UTF-8 boundary.
/// Appends "... (truncated)" when truncation occurs.
fn truncate_string(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... (truncated)", &s[..end])
}

/// Truncate a JSON value to at most `max_bytes` when serialized.
/// Objects/arrays are serialized then truncated as a string; scalars are
/// returned as-is if they fit.
fn truncate_json_value(v: &serde_json::Value, max_bytes: usize) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(truncate_string(s, max_bytes)),
        other => {
            let serialized = serde_json::to_string(other).unwrap_or_default();
            if serialized.len() <= max_bytes {
                other.clone()
            } else {
                serde_json::Value::String(truncate_string(&serialized, max_bytes))
            }
        }
    }
}

/// Sanitize arguments and result fields for hook payloads.
/// For sensitive tools, replaces content with a redaction notice.
/// For other tools, truncates to `MAX_PAYLOAD_FIELD_BYTES`.
fn sanitize_payload(
    tool_name: Option<&str>,
    arguments: Option<serde_json::Value>,
    result: Option<String>,
) -> (Option<serde_json::Value>, Option<String>) {
    let is_sensitive = tool_name
        .map(|n| SENSITIVE_TOOLS.contains(&n))
        .unwrap_or(false);

    let sanitized_args = arguments.map(|args| {
        if is_sensitive {
            serde_json::json!({"redacted": true, "reason": "sensitive tool"})
        } else {
            truncate_json_value(&args, MAX_PAYLOAD_FIELD_BYTES)
        }
    });

    let sanitized_result = result.map(|r| {
        if is_sensitive {
            "[redacted: sensitive tool output]".to_string()
        } else {
            truncate_string(&r, MAX_PAYLOAD_FIELD_BYTES)
        }
    });

    (sanitized_args, sanitized_result)
}

impl HookPayload {
    /// Payload for a before-LLM-call hook.
    pub fn before_llm(
        model: &str,
        message_count: usize,
        iteration: u32,
        ctx: Option<&HookContext>,
    ) -> Self {
        let mut p = Self {
            event: HookEvent::BeforeLlmCall,
            message_count: Some(message_count),
            model: Some(model.to_string()),
            iteration: Some(iteration),
            ..Self::empty(HookEvent::BeforeLlmCall)
        };
        p.apply_context(ctx);
        p
    }

    /// Payload for an after-LLM-call hook.
    #[allow(clippy::too_many_arguments)]
    pub fn after_llm(
        model: &str,
        iteration: u32,
        stop_reason: &str,
        has_tool_calls: bool,
        input_tokens: u32,
        output_tokens: u32,
        provider_name: &str,
        latency_ms: u64,
        cumulative_input_tokens: u32,
        cumulative_output_tokens: u32,
        session_cost: Option<f64>,
        response_cost: Option<f64>,
        ctx: Option<&HookContext>,
    ) -> Self {
        let mut p = Self {
            event: HookEvent::AfterLlmCall,
            model: Some(model.to_string()),
            iteration: Some(iteration),
            stop_reason: Some(stop_reason.to_string()),
            has_tool_calls: Some(has_tool_calls),
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            provider_name: Some(provider_name.to_string()),
            latency_ms: Some(latency_ms),
            cumulative_input_tokens: Some(cumulative_input_tokens),
            cumulative_output_tokens: Some(cumulative_output_tokens),
            session_cost,
            response_cost,
            ..Self::empty(HookEvent::AfterLlmCall)
        };
        p.apply_context(ctx);
        p
    }

    /// Payload for a before-tool-call hook.
    ///
    /// Arguments are sanitized: sensitive tools are redacted, others truncated
    /// to 1 KB to prevent secrets from leaking to hook processes.
    pub fn before_tool(
        name: &str,
        arguments: serde_json::Value,
        tool_id: &str,
        ctx: Option<&HookContext>,
    ) -> Self {
        let (sanitized_args, _) = sanitize_payload(Some(name), Some(arguments), None);
        let mut p = Self {
            event: HookEvent::BeforeToolCall,
            tool_name: Some(name.to_string()),
            arguments: sanitized_args,
            tool_id: Some(tool_id.to_string()),
            ..Self::empty(HookEvent::BeforeToolCall)
        };
        p.apply_context(ctx);
        p
    }

    /// Payload for an after-tool-call hook.
    ///
    /// Result is sanitized: sensitive tools are redacted, others truncated
    /// to 1 KB to prevent secrets from leaking to hook processes.
    pub fn after_tool(
        name: &str,
        tool_id: &str,
        result: String,
        success: bool,
        duration_ms: u64,
        ctx: Option<&HookContext>,
    ) -> Self {
        let (_, sanitized_result) = sanitize_payload(Some(name), None, Some(result));
        let mut p = Self {
            event: HookEvent::AfterToolCall,
            tool_name: Some(name.to_string()),
            tool_id: Some(tool_id.to_string()),
            result: sanitized_result,
            success: Some(success),
            duration_ms: Some(duration_ms),
            ..Self::empty(HookEvent::AfterToolCall)
        };
        p.apply_context(ctx);
        p
    }

    fn apply_context(&mut self, ctx: Option<&HookContext>) {
        if let Some(ctx) = ctx {
            self.session_id.clone_from(&ctx.session_id);
            self.profile_id.clone_from(&ctx.profile_id);
        }
    }

    fn empty(event: HookEvent) -> Self {
        Self {
            event,
            tool_name: None,
            arguments: None,
            tool_id: None,
            result: None,
            success: None,
            duration_ms: None,
            message_count: None,
            model: None,
            iteration: None,
            stop_reason: None,
            has_tool_calls: None,
            input_tokens: None,
            output_tokens: None,
            session_id: None,
            profile_id: None,
            cumulative_input_tokens: None,
            cumulative_output_tokens: None,
            session_cost: None,
            response_cost: None,
            provider_name: None,
            latency_ms: None,
        }
    }
}

/// Result of running hooks for an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookResult {
    /// All hooks passed (or no hooks matched).
    Allow,
    /// A before-hook denied the operation.
    Deny(String),
    /// A before-hook modified the tool arguments (exit code 2, stdout = new args JSON).
    /// Like Claude Agent SDK's `updatedInput` pattern.
    Modified(serde_json::Value),
    /// A hook encountered an error (does not block).
    Error(String),
}

/// Executes hooks with circuit breaker protection.
pub struct HookExecutor {
    hooks: Vec<HookConfig>,
    /// Per-hook consecutive failure count.
    failures: Vec<AtomicU32>,
    failure_threshold: u32,
}

impl HookExecutor {
    pub fn new(hooks: Vec<HookConfig>) -> Self {
        Self::with_threshold(hooks, 3)
    }

    pub fn with_threshold(hooks: Vec<HookConfig>, failure_threshold: u32) -> Self {
        let failures = (0..hooks.len()).map(|_| AtomicU32::new(0)).collect();
        Self {
            hooks,
            failures,
            failure_threshold,
        }
    }

    /// Run all matching hooks for the given event sequentially.
    /// Returns `Deny` on the first before-hook that exits with 1.
    pub async fn run(&self, event: HookEvent, payload: &HookPayload) -> HookResult {
        let payload_json = match serde_json::to_string(payload) {
            Ok(j) => j,
            Err(e) => return HookResult::Error(format!("failed to serialize payload: {e}")),
        };

        let mut last_error = None;

        for (i, hook) in self.hooks.iter().enumerate() {
            if hook.event != event {
                continue;
            }

            // Apply tool_filter for tool events
            if matches!(event, HookEvent::BeforeToolCall | HookEvent::AfterToolCall)
                && !hook.tool_filter.is_empty()
            {
                let tool_name = payload.tool_name.as_deref().unwrap_or("");
                if !hook.tool_filter.iter().any(|f| f == tool_name) {
                    continue;
                }
            }

            // Circuit breaker: skip if too many failures
            let fail_count = self.failures[i].load(Ordering::Relaxed);
            if fail_count >= self.failure_threshold {
                // Atomically claim the warning (threshold -> threshold+1) so it fires once
                if self.failures[i]
                    .compare_exchange(
                        self.failure_threshold,
                        self.failure_threshold + 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    warn!(
                        hook_command = ?hook.command,
                        "hook disabled after {} consecutive failures",
                        self.failure_threshold
                    );
                }
                continue;
            }

            match self.execute_hook(hook, &payload_json).await {
                Ok((0, _stdout)) => {
                    self.failures[i].store(0, Ordering::Relaxed);
                }
                Ok((1, stdout)) => {
                    if matches!(event, HookEvent::BeforeToolCall | HookEvent::BeforeLlmCall) {
                        self.failures[i].store(0, Ordering::Relaxed);
                        return HookResult::Deny(stdout);
                    }
                    // Exit 1 on after-hooks is an error (deny is meaningless for after-events)
                    let new_count = self.failures[i].fetch_add(1, Ordering::Relaxed) + 1;
                    let msg = format!(
                        "hook {:?} exited with code 1 on after-event ({}/{})",
                        hook.command, new_count, self.failure_threshold
                    );
                    warn!("{}", msg);
                    last_error = Some(msg);
                }
                Ok((2, stdout)) => {
                    // Exit 2 = modified args (before-hooks only).
                    // Stdout contains the modified tool arguments as JSON.
                    if matches!(event, HookEvent::BeforeToolCall) {
                        self.failures[i].store(0, Ordering::Relaxed);
                        match serde_json::from_str::<serde_json::Value>(&stdout) {
                            Ok(modified_args) => {
                                tracing::info!(
                                    hook_command = ?hook.command,
                                    "hook modified tool arguments"
                                );
                                return HookResult::Modified(modified_args);
                            }
                            Err(e) => {
                                warn!(
                                    hook_command = ?hook.command,
                                    error = %e,
                                    "hook exit 2 but stdout is not valid JSON, treating as error"
                                );
                                last_error =
                                    Some(format!("hook modified output not valid JSON: {e}"));
                            }
                        }
                    } else {
                        let new_count = self.failures[i].fetch_add(1, Ordering::Relaxed) + 1;
                        let msg = format!(
                            "hook {:?} exited with code 2 on non-before-tool event ({}/{})",
                            hook.command, new_count, self.failure_threshold
                        );
                        warn!("{}", msg);
                        last_error = Some(msg);
                    }
                }
                Ok((code, _stdout)) => {
                    let new_count = self.failures[i].fetch_add(1, Ordering::Relaxed) + 1;
                    let msg = format!(
                        "hook {:?} exited with code {} ({}/{})",
                        hook.command, code, new_count, self.failure_threshold
                    );
                    warn!("{}", msg);
                    last_error = Some(msg);
                }
                Err(e) => {
                    let new_count = self.failures[i].fetch_add(1, Ordering::Relaxed) + 1;
                    let msg = format!(
                        "hook {:?} failed: {} ({}/{})",
                        hook.command, e, new_count, self.failure_threshold
                    );
                    warn!("{}", msg);
                    last_error = Some(msg);
                }
            }
        }

        if let Some(err) = last_error {
            HookResult::Error(err)
        } else {
            HookResult::Allow
        }
    }

    /// Execute a single hook process. Returns (exit_code, stdout).
    async fn execute_hook(
        &self,
        hook: &HookConfig,
        payload_json: &str,
    ) -> eyre::Result<(i32, String)> {
        let (program, args) = hook
            .command
            .split_first()
            .ok_or_else(|| eyre::eyre!("empty hook command"))?;

        // Expand ~ to home directory in program and all arguments
        let program = expand_tilde(program);
        let expanded_args: Vec<String> = args.iter().map(|a| expand_tilde(a)).collect();

        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&expanded_args);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Sanitize environment
        for var in BLOCKED_ENV_VARS {
            cmd.env_remove(var);
        }

        let mut child = cmd.spawn()?;

        // Write payload to stdin inline (payload is small JSON, no need to spawn)
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload_json.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }

        // Take stdout/stderr handles so we can read them after wait
        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        // Wait with timeout (use wait() instead of wait_with_output() so child isn't consumed)
        let timeout = Duration::from_millis(hook.timeout_ms);
        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => {
                let stdout = if let Some(mut handle) = stdout_handle {
                    let mut buf = Vec::new();
                    let _ = handle.read_to_end(&mut buf).await;
                    String::from_utf8_lossy(&buf).trim().to_string()
                } else {
                    String::new()
                };
                // Log stderr from the hook process (diagnostic output)
                if let Some(mut handle) = stderr_handle {
                    let mut buf = Vec::new();
                    let _ = handle.read_to_end(&mut buf).await;
                    let stderr = String::from_utf8_lossy(&buf);
                    for line in stderr.lines() {
                        let line = line.trim();
                        if !line.is_empty() {
                            tracing::info!(
                                hook = ?hook.command,
                                "{line}"
                            );
                        }
                    }
                }
                let code = status.code().unwrap_or(2);
                tracing::info!(
                    hook = ?hook.command,
                    exit_code = code,
                    stdout_len = stdout.len(),
                    "hook executed"
                );
                Ok((code, stdout))
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                // Timeout: kill the child process to prevent orphans
                let _ = child.kill().await;
                Err(eyre::eyre!("hook timed out after {}ms", hook.timeout_ms))
            }
        }
    }
}

/// Expand leading `~` or `~/` to the user's home directory.
/// Also handles `~username/` by looking up `/home/username` (Unix) or
/// `/Users/username` (macOS).
fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}{}", home.display(), &path[1..]);
        }
    } else if let Some(rest) = path.strip_prefix('~') {
        // ~username or ~username/...
        let (username, suffix) = match rest.find('/') {
            Some(pos) => (&rest[..pos], &rest[pos..]),
            None => (rest, ""),
        };
        // Reject usernames with path traversal or unsafe characters.
        // Only allow alphanumeric, hyphen, underscore, and dot (no leading dot).
        // This allowlist implicitly blocks path separators (/ \), null bytes,
        // and other injection characters on all platforms.
        let is_safe_username = !username.is_empty()
            && !username.starts_with('.')
            && username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
        if !is_safe_username {
            warn!(
                path,
                username, "tilde expansion blocked: invalid username, returning path as-is"
            );
            return path.to_string();
        }
        #[cfg(target_os = "macos")]
        let home_base = "/Users";
        #[cfg(windows)]
        let home_base = {
            let drive = std::env::var("SYSTEMDRIVE").unwrap_or_else(|_| "C:".to_string());
            format!("{drive}\\Users")
        };
        #[cfg(not(any(target_os = "macos", windows)))]
        let home_base = "/home";
        #[cfg(windows)]
        return format!("{}\\{}{}", home_base, username, suffix);
        #[cfg(not(windows))]
        return format!("{}/{}{}", home_base, username, suffix);
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_config_deserialize() {
        let json = r#"{
            "event": "before_tool_call",
            "command": ["python3", "~/.octos/hooks/audit.py"],
            "timeout_ms": 3000,
            "tool_filter": ["shell", "write_file"]
        }"#;
        let hook: HookConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hook.event, HookEvent::BeforeToolCall);
        assert_eq!(hook.command, vec!["python3", "~/.octos/hooks/audit.py"]);
        assert_eq!(hook.timeout_ms, 3000);
        assert_eq!(hook.tool_filter, vec!["shell", "write_file"]);
    }

    #[test]
    fn test_hook_config_defaults() {
        let json = r#"{
            "event": "after_llm_call",
            "command": ["echo", "done"]
        }"#;
        let hook: HookConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hook.timeout_ms, 5000);
        assert!(hook.tool_filter.is_empty());
    }

    #[test]
    fn test_payload_serialization() {
        let payload = HookPayload::before_tool(
            "shell",
            serde_json::json!({"command": "ls"}),
            "call_1",
            None,
        );
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"event\":\"before_tool_call\""));
        assert!(json.contains("\"tool_name\":\"shell\""));
        assert!(!json.contains("\"result\""));
        assert!(!json.contains("\"success\""));
        // No context — session_id/profile_id should be absent
        assert!(!json.contains("\"session_id\""));
        assert!(!json.contains("\"profile_id\""));
    }

    #[test]
    fn test_payload_constructors() {
        let before_llm = HookPayload::before_llm("gpt-4", 10, 3, None);
        assert_eq!(before_llm.event, HookEvent::BeforeLlmCall);
        assert_eq!(before_llm.model.as_deref(), Some("gpt-4"));
        assert_eq!(before_llm.message_count, Some(10));
        assert_eq!(before_llm.iteration, Some(3));
        assert!(before_llm.tool_name.is_none());
        assert!(before_llm.session_id.is_none());

        let after_llm = HookPayload::after_llm(
            "gpt-4",
            3,
            "EndTurn",
            false,
            100,
            50,
            "openai",
            1234,
            500,
            200,
            Some(0.05),
            Some(0.01),
            None,
        );
        assert_eq!(after_llm.event, HookEvent::AfterLlmCall);
        assert_eq!(after_llm.input_tokens, Some(100));
        assert_eq!(after_llm.has_tool_calls, Some(false));
        assert_eq!(after_llm.provider_name.as_deref(), Some("openai"));
        assert_eq!(after_llm.latency_ms, Some(1234));
        assert_eq!(after_llm.cumulative_input_tokens, Some(500));
        assert_eq!(after_llm.cumulative_output_tokens, Some(200));
        assert_eq!(after_llm.session_cost, Some(0.05));
        assert_eq!(after_llm.response_cost, Some(0.01));

        let after_tool = HookPayload::after_tool("shell", "tc1", "ok".into(), true, 42, None);
        assert_eq!(after_tool.event, HookEvent::AfterToolCall);
        assert_eq!(after_tool.success, Some(true));
        assert_eq!(after_tool.duration_ms, Some(42));
    }

    #[test]
    fn test_payload_with_hook_context() {
        let ctx = HookContext {
            session_id: Some("sess-123".into()),
            profile_id: Some("prof-abc".into()),
        };
        let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", Some(&ctx));
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"session_id\":\"sess-123\""));
        assert!(json.contains("\"profile_id\":\"prof-abc\""));
    }

    #[test]
    fn test_after_llm_enriched_payload() {
        let ctx = HookContext {
            session_id: Some("s1".into()),
            profile_id: Some("p1".into()),
        };
        let payload = HookPayload::after_llm(
            "kimi-2.5",
            5,
            "ToolUse",
            true,
            200,
            80,
            "moonshot",
            3456,
            1000,
            400,
            Some(0.12),
            Some(0.03),
            Some(&ctx),
        );
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"provider_name\":\"moonshot\""));
        assert!(json.contains("\"latency_ms\":3456"));
        assert!(json.contains("\"cumulative_input_tokens\":1000"));
        assert!(json.contains("\"cumulative_output_tokens\":400"));
        assert!(json.contains("\"session_cost\":0.12"));
        assert!(json.contains("\"response_cost\":0.03"));
        assert!(json.contains("\"session_id\":\"s1\""));
    }

    #[tokio::test]
    async fn test_circuit_breaker_tracking() {
        // A hook at the failure threshold should be skipped (not executed).
        // Use a command that would fail if actually run.
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec!["false".into()], // would fail if executed
            timeout_ms: 1000,
            tool_filter: vec![],
        }]);
        // Set failures at threshold so circuit breaker trips
        executor.failures[0].store(3, Ordering::Relaxed);

        let payload = HookPayload {
            event: HookEvent::AfterToolCall,
            tool_name: Some("test".into()),
            arguments: None,
            tool_id: None,
            result: None,
            success: None,
            duration_ms: None,
            message_count: None,
            model: None,
            iteration: None,
            stop_reason: None,
            has_tool_calls: None,
            input_tokens: None,
            output_tokens: None,
            session_id: None,
            profile_id: None,
            cumulative_input_tokens: None,
            cumulative_output_tokens: None,
            session_cost: None,
            response_cost: None,
            provider_name: None,
            latency_ms: None,
        };
        let result = executor.run(HookEvent::AfterToolCall, &payload).await;
        // Hook should be skipped (circuit broken), not denied
        assert!(matches!(result, HookResult::Allow));
    }

    #[test]
    fn test_tool_filter_config() {
        let hook = HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["check".into()],
            timeout_ms: 1000,
            tool_filter: vec!["shell".into(), "write_file".into()],
        };
        assert!(hook.tool_filter.contains(&"shell".to_string()));
        assert!(!hook.tool_filter.contains(&"read_file".to_string()));
    }

    #[test]
    fn test_expand_tilde() {
        let expanded = expand_tilde("~/foo/bar");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.contains("foo/bar") || expanded.contains("foo\\bar"));

        // ~username expansion
        let expanded = expand_tilde("~alice/scripts/hook.sh");
        assert!(expanded.contains("alice"));
        assert!(expanded.ends_with("/scripts/hook.sh"));
        assert!(!expanded.starts_with('~'));

        // ~username without trailing path
        let expanded = expand_tilde("~bob");
        assert!(expanded.contains("bob"));

        // Non-tilde paths unchanged
        assert_eq!(expand_tilde("/usr/bin/foo"), "/usr/bin/foo");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn test_expand_tilde_rejects_traversal() {
        // Path traversal via username must return the path unexpanded
        assert_eq!(expand_tilde("~../../bin/evil"), "~../../bin/evil");
        assert_eq!(expand_tilde("~../etc/passwd"), "~../etc/passwd");
        assert_eq!(expand_tilde("~.hidden/path"), "~.hidden/path");
    }

    #[test]
    fn test_expand_tilde_rejects_unsafe_chars() {
        // Null bytes and backslashes in username are blocked by the allowlist
        assert_eq!(expand_tilde("~user\0evil"), "~user\0evil");
        assert_eq!(expand_tilde("~user\\evil"), "~user\\evil");
        assert_eq!(expand_tilde("~user:evil"), "~user:evil");
        assert_eq!(expand_tilde("~ spaces"), "~ spaces");
    }

    #[test]
    fn test_expand_tilde_allows_valid_usernames() {
        let expanded = expand_tilde("~valid-user_1/path");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.contains("valid-user_1"));

        let expanded = expand_tilde("~user.name/path");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.contains("user.name"));
    }

    #[tokio::test]
    async fn test_executor_no_hooks() {
        let executor = HookExecutor::new(vec![]);
        let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_executor_allow_hook() {
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["true".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
        }]);
        let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_executor_deny_hook() {
        // `false` exits with code 1
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
        }]);
        let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert!(matches!(result, HookResult::Deny(_)));
    }

    #[tokio::test]
    async fn test_executor_tool_filter_skips() {
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec!["write_file".into()],
        }]);
        let payload = HookPayload::before_tool("read_file", serde_json::json!({}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    async fn test_executor_event_mismatch_skips() {
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
        }]);
        let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_circuit_breaker_below_threshold_still_runs() {
        // After-tool hook that exits with code 2 (error, not deny)
        let executor = HookExecutor::with_threshold(
            vec![HookConfig {
                event: HookEvent::AfterToolCall,
                command: vec!["sh".into(), "-c".into(), "exit 2".into()],
                timeout_ms: 5000,
                tool_filter: vec![],
            }],
            3,
        );
        let payload = HookPayload::after_tool("shell", "tc1", "ok".into(), true, 10, None);

        // First two failures: hook still runs (returns Error, not Allow)
        let r1 = executor.run(HookEvent::AfterToolCall, &payload).await;
        assert!(matches!(r1, HookResult::Error(_)));
        let r2 = executor.run(HookEvent::AfterToolCall, &payload).await;
        assert!(matches!(r2, HookResult::Error(_)));
        assert_eq!(executor.failures[0].load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_circuit_breaker_at_threshold_disables() {
        let executor = HookExecutor::with_threshold(
            vec![HookConfig {
                event: HookEvent::AfterToolCall,
                command: vec!["sh".into(), "-c".into(), "exit 2".into()],
                timeout_ms: 5000,
                tool_filter: vec![],
            }],
            3,
        );
        let payload = HookPayload::after_tool("shell", "tc1", "ok".into(), true, 10, None);

        // Trigger 3 failures to hit threshold
        for _ in 0..3 {
            executor.run(HookEvent::AfterToolCall, &payload).await;
        }

        // Fourth call: hook is disabled (skipped), returns Allow
        let r = executor.run(HookEvent::AfterToolCall, &payload).await;
        assert_eq!(r, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_circuit_breaker_resets_on_success() {
        let executor = HookExecutor::with_threshold(
            vec![HookConfig {
                event: HookEvent::AfterToolCall,
                command: vec!["true".into()],
                timeout_ms: 5000,
                tool_filter: vec![],
            }],
            3,
        );

        // Simulate 2 prior failures
        executor.failures[0].store(2, Ordering::Relaxed);

        // Success resets counter
        let payload = HookPayload::after_tool("shell", "tc1", "ok".into(), true, 10, None);
        let r = executor.run(HookEvent::AfterToolCall, &payload).await;
        assert_eq!(r, HookResult::Allow);
        assert_eq!(executor.failures[0].load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_truncate_string_short() {
        assert_eq!(truncate_string("hello", 1024), "hello");
    }

    #[test]
    fn test_truncate_string_long() {
        let long = "x".repeat(2000);
        let result = truncate_string(&long, 1024);
        assert!(result.len() < 1100); // 1024 + "... (truncated)"
        assert!(result.ends_with("... (truncated)"));
    }

    #[test]
    fn test_truncate_string_utf8_boundary() {
        // Multi-byte char: each is 3 bytes
        let s = "\u{4e16}\u{754c}"; // 6 bytes total
        let result = truncate_string(s, 4);
        // Should cut at char boundary (3), not at 4
        assert!(result.contains("... (truncated)"));
    }

    #[test]
    fn test_sensitive_tool_before_redacted() {
        let payload = HookPayload::before_tool(
            "shell",
            serde_json::json!({"command": "cat /etc/passwd"}),
            "tc1",
            None,
        );
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"redacted\":true"));
        assert!(!json.contains("/etc/passwd"));
    }

    #[test]
    fn test_sensitive_tool_after_redacted() {
        let payload = HookPayload::after_tool(
            "read_file",
            "tc1",
            "SECRET_KEY=hunter2\nDB_PASS=abc".into(),
            true,
            10,
            None,
        );
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("redacted"));
        assert!(!json.contains("hunter2"));
    }

    #[test]
    fn test_nonsensitive_tool_truncated_not_redacted() {
        let big_args = serde_json::json!({"data": "x".repeat(2000)});
        let payload = HookPayload::before_tool("glob", big_args, "tc1", None);
        let json = serde_json::to_string(&payload).unwrap();
        // Should be truncated, not redacted
        assert!(json.contains("truncated"));
        assert!(!json.contains("\"redacted\""));
    }

    #[test]
    fn test_nonsensitive_tool_small_payload_unchanged() {
        let payload =
            HookPayload::before_tool("glob", serde_json::json!({"pattern": "*.rs"}), "tc1", None);
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("*.rs"));
        assert!(!json.contains("truncated"));
        assert!(!json.contains("redacted"));
    }
}
