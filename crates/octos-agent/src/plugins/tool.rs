//! Plugin tool: wraps a plugin executable as a Tool.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::progress::ProgressEvent;
use crate::tools::{TOOL_CTX, Tool, ToolContext, ToolResult};

use super::manifest::PluginToolDef;

/// A tool backed by a plugin executable.
///
/// Protocol: write JSON args to stdin, read JSON result from stdout.
/// Expected output: `{ "output": "...", "success": true/false }`
pub struct PluginTool {
    plugin_name: String,
    tool_def: PluginToolDef,
    executable: PathBuf,
    /// Environment variables to strip from the plugin's environment.
    blocked_env: Vec<String>,
    /// Extra environment variables to inject into the plugin's environment.
    extra_env: Vec<(String, String)>,
    /// Working directory for plugin execution (created on first use).
    work_dir: Option<PathBuf>,
    /// Execution timeout.
    timeout: Duration,
}

impl PluginTool {
    /// Default timeout for plugin execution.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

    pub fn new(plugin_name: String, tool_def: PluginToolDef, executable: PathBuf) -> Self {
        Self {
            plugin_name,
            tool_def,
            executable,
            blocked_env: vec![],
            extra_env: vec![],
            work_dir: None,
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Set environment variables to block from plugin execution.
    pub fn with_blocked_env(mut self, blocked: Vec<String>) -> Self {
        self.blocked_env = blocked;
        self
    }

    /// Set extra environment variables to inject into plugin execution.
    pub fn with_extra_env(mut self, env: Vec<(String, String)>) -> Self {
        self.extra_env = env;
        self
    }

    /// Set the working directory for plugin processes.
    /// The directory is created automatically if it doesn't exist.
    pub fn with_work_dir(mut self, dir: PathBuf) -> Self {
        self.work_dir = Some(dir);
        self
    }

    /// Set custom execution timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Create a copy of this plugin tool with a different work directory.
    /// Used to give each user session its own workspace for plugin output.
    pub fn clone_with_work_dir(&self, work_dir: PathBuf) -> Self {
        Self {
            plugin_name: self.plugin_name.clone(),
            tool_def: self.tool_def.clone(),
            executable: self.executable.clone(),
            blocked_env: self.blocked_env.clone(),
            extra_env: self.extra_env.clone(),
            work_dir: Some(work_dir),
            timeout: self.timeout,
        }
    }
}

#[async_trait]
impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.tool_def.name
    }

    fn description(&self) -> &str {
        &self.tool_def.description
    }

    fn input_schema(&self) -> serde_json::Value {
        let mut schema = self.tool_def.input_schema.clone();
        // Inject `timeout_secs` so the LLM can request longer timeouts for
        // complex tasks.  Only added when the schema is an object with
        // "properties" and doesn't already define the field.
        if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            if !props.contains_key("timeout_secs") {
                props.insert(
                    "timeout_secs".to_string(),
                    serde_json::json!({
                        "type": "integer",
                        "description": "Timeout in seconds. Estimate based on real execution times: single deep_search (depth=2) ~3min → 300s; single deep_search (depth=3) ~5min → 400s; research pipeline with 3 topics ~8min → 600s; research pipeline with 5-7 topics ~15-20min → 1200s; very complex multi-source analysis ~25min → 1500s. Max: 1800. Default: 600"
                    }),
                );
            }
        }
        schema
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        tracing::info!(
            plugin = %self.plugin_name,
            tool = %self.tool_def.name,
            executable = %self.executable.display(),
            timeout_secs = self.timeout.as_secs(),
            args_size = args.to_string().len(),
            "spawning plugin process"
        );

        let mut cmd = Command::new(&self.executable);
        cmd.arg(&self.tool_def.name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Remove blocked environment variables
        for var in &self.blocked_env {
            cmd.env_remove(var);
        }

        // Inject extra environment variables (e.g. provider base URLs, API keys)
        for (key, val) in &self.extra_env {
            cmd.env(key, val);
        }

        // Expose a work directory for plugin output files via OCTOS_WORK_DIR.
        // Plugins find style/config files relative to their executable location
        // (via std::env::current_exe()), not cwd.  The OS cwd is inherited from
        // the gateway subprocess which is already narrowed to data_dir by
        // process_manager.rs (.current_dir(&data_dir)).
        if let Some(ref dir) = self.work_dir {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "failed to create plugin work_dir"
                );
            }
            cmd.env("OCTOS_WORK_DIR", dir);
        }

        let mut child = cmd.spawn().wrap_err_with(|| {
            format!(
                "failed to spawn plugin '{}' executable: {}",
                self.plugin_name,
                self.executable.display()
            )
        })?;

        let child_pid = child.id().unwrap_or(0);
        tracing::info!(
            plugin = %self.plugin_name,
            tool = %self.tool_def.name,
            pid = child_pid,
            "plugin process spawned"
        );

        // Inject defaults for known plugins
        let mut effective_args = args.clone();
        if self.tool_def.name == "mofa_slides" {
            if let Some(obj) = effective_args.as_object_mut() {
                if !obj.contains_key("out")
                    || obj["out"].as_str().map(|s| s.is_empty()).unwrap_or(true)
                {
                    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
                    obj.insert(
                        "out".into(),
                        serde_json::Value::String(format!("slides_{ts}.pptx")),
                    );
                    tracing::info!("injected default 'out' for mofa_slides");
                }
            }
        }

        // Write args to stdin
        if let Some(mut stdin) = child.stdin.take() {
            let data = serde_json::to_vec(&effective_args)?;
            stdin.write_all(&data).await?;
            // Drop stdin to signal EOF
        }

        // Take stdout and stderr handles for separate streaming
        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        // Read tool context from task-local (set by agent.rs)
        let ctx: Option<ToolContext> = TOOL_CTX.try_with(|c| c.clone()).ok();

        // Spawn stderr reader: streams lines as ToolProgress events
        let tool_name = self.tool_def.name.clone();
        let stderr_task = tokio::spawn(async move {
            let mut collected = String::new();
            if let Some(stderr) = stderr_handle {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Some(ref ctx) = ctx {
                        ctx.reporter.report(ProgressEvent::ToolProgress {
                            name: tool_name.clone(),
                            tool_id: ctx.tool_id.clone(),
                            message: line.clone(),
                        });
                    }
                    if !collected.is_empty() {
                        collected.push('\n');
                    }
                    collected.push_str(&line);
                }
            }
            collected
        });

        // Spawn stdout reader: buffers full output for result parsing
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut stdout) = stdout_handle {
                let _ = stdout.read_to_end(&mut buf).await;
            }
            buf
        });

        // Wait for process exit with timeout
        let exit_status = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                stderr_task.abort();
                stdout_task.abort();
                return Err(eyre::eyre!(
                    "plugin '{}' tool '{}' execution failed: {e}",
                    self.plugin_name,
                    self.tool_def.name
                ));
            }
            Err(_) => {
                // Timeout — kill the child process and abort reader tasks
                stderr_task.abort();
                stdout_task.abort();
                #[cfg(unix)]
                if child_pid > 0 {
                    let _ = std::process::Command::new("kill")
                        .args(["-9", &format!("-{child_pid}")])
                        .status();
                    let _ = std::process::Command::new("kill")
                        .args(["-9", &child_pid.to_string()])
                        .status();
                }
                #[cfg(windows)]
                if child_pid > 0 {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &child_pid.to_string()])
                        .status();
                }
                return Err(eyre::eyre!(
                    "plugin '{}' tool '{}' timed out after {}s",
                    self.plugin_name,
                    self.tool_def.name,
                    self.timeout.as_secs()
                ));
            }
        };

        // Collect stdout and stderr from reader tasks
        let stdout_bytes = stdout_task.await.unwrap_or_default();
        let stderr_text = stderr_task.await.unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout_bytes);

        tracing::info!(
            plugin = %self.plugin_name,
            tool = %self.tool_def.name,
            pid = child_pid,
            exit_code = exit_status.code().unwrap_or(-1),
            stdout_len = stdout.len(),
            stderr_len = stderr_text.len(),
            "plugin process completed"
        );

        // Try to parse structured output
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&stdout) {
            let output = parsed
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or(&stdout)
                .to_string();
            let success = parsed
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(exit_status.success());
            // Check if plugin reported a file path
            let file_modified = parsed
                .get("file_modified")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    // Detect "Report saved to: <path>" pattern in output
                    output.lines().find_map(|line| {
                        line.strip_prefix("Report saved to: ")
                            .or_else(|| line.strip_prefix("Report saved to:"))
                            .map(|p| std::path::PathBuf::from(p.trim()))
                    })
                });
            // Parse files_to_send: plugin can request auto-delivery to chat
            let mut files_to_send: Vec<std::path::PathBuf> = parsed
                .get("files_to_send")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(std::path::PathBuf::from))
                        .collect()
                })
                .unwrap_or_default();

            // Auto-deliver output file when plugin didn't report it.
            // Check multiple locations: work_dir, cwd, and the output text itself.
            let file_modified = if file_modified.is_none() && files_to_send.is_empty() {
                // Try from `out` arg
                let out_file = effective_args
                    .get("out")
                    .and_then(|v| v.as_str())
                    .and_then(|p| {
                        let path = std::path::PathBuf::from(p);
                        if path.is_absolute() && path.exists() {
                            return Some(path);
                        }
                        // Try work_dir, then cwd
                        let candidates: Vec<std::path::PathBuf> = [
                            self.work_dir.as_ref().map(|d| d.join(&path)),
                            std::env::current_dir().ok().map(|d| d.join(&path)),
                        ]
                        .into_iter()
                        .flatten()
                        .collect();
                        candidates.into_iter().find(|c| c.exists())
                    });
                // Also try parsing file path from output text (e.g. "Generated PPTX: path.pptx")
                let from_output = if out_file.is_none() {
                    output.lines().find_map(|line| {
                        line.strip_prefix("Generated PPTX: ")
                            .or_else(|| line.strip_prefix("Generated: "))
                            .map(|p| std::path::PathBuf::from(p.trim()))
                            .and_then(|path| {
                                if path.exists() {
                                    return Some(path.clone());
                                }
                                let in_work = self.work_dir.as_ref().map(|d| d.join(&path));
                                let in_cwd = std::env::current_dir().ok().map(|d| d.join(&path));
                                in_work
                                    .filter(|p| p.exists())
                                    .or_else(|| in_cwd.filter(|p| p.exists()))
                            })
                    })
                } else {
                    None
                };
                let found = out_file.or(from_output);
                if let Some(ref abs) = found {
                    tracing::info!(file = %abs.display(), "auto-detected output file for delivery");
                    files_to_send.push(abs.clone());
                }
                found
            } else {
                file_modified
            };

            return Ok(ToolResult {
                output,
                success,
                file_modified,
                files_to_send,
                ..Default::default()
            });
        }

        // Fallback: raw stdout + stderr
        let mut output = stdout.to_string();
        if !stderr_text.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&stderr_text);
        }

        Ok(ToolResult {
            output,
            success: exit_status.success(),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_tool_def(name: &str, desc: &str) -> PluginToolDef {
        PluginToolDef {
            name: name.to_string(),
            description: desc.to_string(),
            input_schema: json!({"type": "object", "properties": {"msg": {"type": "string"}}}),
            spawn_only: false,
            spawn_only_message: None,
        }
    }

    #[test]
    fn new_sets_defaults() {
        let def = make_tool_def("greet", "Say hello");
        let tool = PluginTool::new("my-plugin".into(), def, PathBuf::from("/bin/echo"));

        assert_eq!(tool.plugin_name, "my-plugin");
        assert_eq!(tool.timeout, PluginTool::DEFAULT_TIMEOUT);
        assert_eq!(tool.timeout, Duration::from_secs(600));
        assert!(tool.blocked_env.is_empty());
    }

    #[test]
    fn with_blocked_env_sets_list() {
        let def = make_tool_def("t", "d");
        let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo"))
            .with_blocked_env(vec!["SECRET".into(), "TOKEN".into()]);

        assert_eq!(tool.blocked_env, vec!["SECRET", "TOKEN"]);
    }

    #[test]
    fn with_extra_env_sets_vars() {
        let def = make_tool_def("t", "d");
        let tool =
            PluginTool::new("p".into(), def, PathBuf::from("/bin/echo")).with_extra_env(vec![
                (
                    "GEMINI_BASE_URL".into(),
                    "https://api.r9s.ai/gemini/v1beta".into(),
                ),
                ("GEMINI_API_KEY".into(), "test-key".into()),
            ]);

        assert_eq!(tool.extra_env.len(), 2);
        assert_eq!(tool.extra_env[0].0, "GEMINI_BASE_URL");
        assert_eq!(tool.extra_env[1].0, "GEMINI_API_KEY");
    }

    #[test]
    fn with_timeout_sets_custom() {
        let def = make_tool_def("t", "d");
        let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo"))
            .with_timeout(Duration::from_secs(120));

        assert_eq!(tool.timeout, Duration::from_secs(120));
    }

    #[test]
    fn trait_methods_delegate_to_tool_def() {
        let def = make_tool_def("my_tool", "A fine tool");
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));

        assert_eq!(tool.name(), "my_tool");
        assert_eq!(tool.description(), "A fine tool");
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["msg"].is_object());
    }

    /// Write a script to a file and make it executable, with fsync to avoid ETXTBSY
    /// on Linux overlayfs (Docker containers).
    #[cfg(unix)]
    fn write_test_script(path: &std::path::Path, content: &str) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.sync_all().unwrap();
        drop(f);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // On Linux overlayfs (Docker), the kernel may still report ETXTBSY
        // briefly after closing. A short sleep allows the inode to settle.
        // macOS doesn't use overlayfs so this is skipped there.
        #[cfg(target_os = "linux")]
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_spawns_subprocess_and_captures_output() {
        // Create a temp script that reads stdin and writes structured JSON to stdout.
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nread INPUT\necho '{\"output\": \"got: '\"$INPUT\"'\", \"success\": true}'\n",
        );

        let def = make_tool_def("echo_tool", "echoes input");
        let tool = PluginTool::new("test-plugin".into(), def, script_path)
            .with_timeout(Duration::from_secs(5));

        let args = json!({"msg": "hello"});
        let result = tool.execute(&args).await.expect("execute should succeed");

        assert!(result.success);
        assert!(
            result.output.contains("got:"),
            "output should contain echoed input, got: {}",
            result.output
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_fallback_on_non_json_stdout() {
        // Script that outputs plain text (not JSON).
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(&script_path, "#!/bin/sh\necho 'plain text output'\n");

        let def = make_tool_def("plain_tool", "plain output");
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let result = tool.execute(&json!({})).await.expect("should succeed");

        assert!(result.success);
        assert!(result.output.contains("plain text output"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_timeout_returns_error() {
        // Skip in Docker containers where pid/process management can cause hangs.
        // This test passes on macOS and bare-metal Linux.
        if std::path::Path::new("/.dockerenv").exists()
            || std::fs::read_to_string("/proc/1/cgroup")
                .map(|s| s.contains("docker") || s.contains("kubepods"))
                .unwrap_or(false)
        {
            eprintln!("skipping execute_timeout_returns_error: container detected");
            return;
        }

        // Script that sleeps longer than the timeout.
        // multi_thread needed because execute() spawns reader tasks that must run
        // concurrently with the timeout future.
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(&script_path, "#!/bin/sh\nsleep 60\n");

        let def = make_tool_def("slow_tool", "too slow");
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(1));

        match tool.execute(&json!({})).await {
            Err(e) => assert!(
                e.to_string().contains("timed out"),
                "expected timeout error, got: {e}"
            ),
            Ok(_) => panic!("expected timeout error, but execute succeeded"),
        }
    }
}
