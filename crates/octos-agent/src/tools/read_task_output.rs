//! `read_task_output` — selective inspection of background task output.
//!
//! M10 Phase 4 (agent context isolation). Mirrors Claude Code's
//! "transcript-file pointer" pattern: when a `spawn_only` tool starts in the
//! background, the LLM only sees a small `task_handle` payload — not the full
//! result. To inspect the result the LLM calls this tool with one of five
//! bounded modes (head/tail/grep/line_range/file). Every mode is capped at
//! ~4 KB so a single call never re-pollutes the context window with a 50 KB
//! research report.
//!
//! The bytes already live on disk via the `SubAgentOutputRouter` (the
//! per-task append-only output file used by the M8.7 dashboard). This tool is
//! a read-only window over that file plus, for `file` mode, the per-task
//! output workspace under the registry's workspace root.
//!
//! Path safety:
//! - The router-managed output file path is computed from the supervisor's
//!   `BackgroundTask::tool_call_id`; the LLM never supplies a path.
//! - For `file` mode the LLM-supplied path is resolved via `resolve_path`
//!   against `workspace_root`, so traversal (`..`) and absolute paths are
//!   rejected. The file is also restricted to one of the task's
//!   `expected_files` if any are recorded.
//! - All reads are bounded by `MAX_OUTPUT_BYTES` to keep agent context lean.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::subagent_output::SubAgentOutputRouter;
use crate::task_supervisor::TaskSupervisor;

use super::{Tool, ToolResult, resolve_path};

/// Hard cap on the bytes any single `read_task_output` call returns. Keeps
/// the LLM context from being re-polluted by large research reports.
pub const MAX_OUTPUT_BYTES: usize = 4 * 1024;

/// Hard cap on the bytes the tool will read off disk before applying the
/// requested mode. Larger than `MAX_OUTPUT_BYTES` so head/tail/grep can scan
/// a meaningful slice of a multi-megabyte log without blowing the heap.
pub const MAX_READ_BYTES: usize = 1024 * 1024;

/// Default line cap for head/tail when the LLM omits one.
pub const DEFAULT_LINE_LIMIT: usize = 50;

/// Hard cap on lines in head/tail/grep/line_range — bounds per-call latency
/// and prevents adversarial line counts from triggering large allocations.
pub const MAX_LINE_LIMIT: usize = 500;

/// Hard cap on grep matches per call.
pub const MAX_GREP_MATCHES: usize = 100;

/// `read_task_output` tool.
pub struct ReadTaskOutputTool {
    supervisor: Arc<TaskSupervisor>,
    session_key: String,
    output_router: Option<Arc<SubAgentOutputRouter>>,
    workspace_root: PathBuf,
}

impl ReadTaskOutputTool {
    /// Build the tool against a per-session supervisor handle, the M8.7
    /// output router, and the user's workspace root (used to resolve
    /// `expected_files` for `file` mode).
    pub fn new(
        supervisor: Arc<TaskSupervisor>,
        session_key: impl Into<String>,
        output_router: Option<Arc<SubAgentOutputRouter>>,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            supervisor,
            session_key: session_key.into(),
            output_router,
            workspace_root: workspace_root.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    task_handle: String,
    #[serde(default)]
    mode: ReadMode,
}

/// Inspection mode.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReadMode {
    /// First N lines of stdout.
    Head {
        #[serde(default = "default_lines")]
        lines: usize,
    },
    /// Last N lines.
    Tail {
        #[serde(default = "default_lines")]
        lines: usize,
    },
    /// Substring scan, capped by `max_matches`.
    Grep {
        pattern: String,
        #[serde(default = "default_grep_matches")]
        max_matches: usize,
    },
    /// Inclusive [start, end] line range, both 1-indexed.
    LineRange { start: usize, end: usize },
    /// Dive into one of the task's `expected_files` and apply the inner mode.
    File {
        path: String,
        #[serde(default)]
        mode: Box<ReadMode>,
    },
}

impl Default for ReadMode {
    fn default() -> Self {
        ReadMode::Head {
            lines: DEFAULT_LINE_LIMIT,
        }
    }
}

fn default_lines() -> usize {
    DEFAULT_LINE_LIMIT
}

fn default_grep_matches() -> usize {
    20
}

#[async_trait]
impl Tool for ReadTaskOutputTool {
    fn name(&self) -> &str {
        "read_task_output"
    }

    fn description(&self) -> &str {
        "Inspect the output of a background task started by a spawn_only tool. \
         Use the `task_handle` returned by the spawn_only call. Modes: head, tail, grep, \
         line_range (over captured stdout), or file (dive into one of the task's expected_files). \
         Every call returns at most ~4KB so agent context stays small. \
         Prefer head:50 first, then grep for specifics, before reading whole files."
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        // Pure read — safe to run alongside other read-only tools.
        super::ConcurrencyClass::Safe
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["task_handle"],
            "properties": {
                "task_handle": {
                    "type": "string",
                    "description": "The task_handle returned by a spawn_only tool's task_handle field."
                },
                "mode": {
                    "type": "object",
                    "description": "Inspection mode. Default: {\"kind\":\"head\",\"lines\":50}.",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["head", "tail", "grep", "line_range", "file"]
                        },
                        "lines": {"type": "integer", "minimum": 1, "maximum": MAX_LINE_LIMIT},
                        "pattern": {"type": "string"},
                        "max_matches": {"type": "integer", "minimum": 1, "maximum": MAX_GREP_MATCHES},
                        "start": {"type": "integer", "minimum": 1},
                        "end": {"type": "integer", "minimum": 1},
                        "path": {"type": "string"},
                        "mode": {"type": "object"}
                    }
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid read_task_output input")?;

        let task = match self.supervisor.get_task(&input.task_handle) {
            Some(t) => t,
            None => {
                return Ok(ToolResult {
                    output: format!("task_handle '{}' not found", input.task_handle),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // Per-session isolation: a session must not be able to read another
        // session's task output even if it guesses a `task_handle`.
        if let Some(ref owner) = task.session_key {
            if owner != &self.session_key {
                return Ok(ToolResult {
                    output: format!(
                        "task_handle '{}' belongs to a different session",
                        input.task_handle
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        }

        let body = match input.mode {
            ReadMode::File { path, mode } => self.read_file(&task, &path, *mode)?,
            inline_mode => {
                let text = self.read_router_text(&task)?;
                apply_mode(&text, inline_mode)?
            }
        };

        let trimmed = truncate_to_cap(body);
        Ok(ToolResult {
            output: trimmed,
            success: true,
            ..Default::default()
        })
    }
}

impl ReadTaskOutputTool {
    fn router_path_for(&self, task: &crate::task_supervisor::BackgroundTask) -> Option<PathBuf> {
        let router = self.output_router.as_ref()?;
        // Mirror the session_id used by `execution.rs` when wiring the router
        // for spawn_only background tasks: `agent:<tool_call_id>`.
        let session_id = format!("agent:{}", task.tool_call_id);
        Some(router.path_for(&session_id, &task.id))
    }

    fn read_router_text(&self, task: &crate::task_supervisor::BackgroundTask) -> Result<String> {
        let path = match self.router_path_for(task) {
            Some(p) => p,
            None => return Ok(String::new()),
        };
        read_capped(&path)
    }

    fn read_file(
        &self,
        task: &crate::task_supervisor::BackgroundTask,
        path: &str,
        mode: ReadMode,
    ) -> Result<String> {
        if matches!(mode, ReadMode::File { .. }) {
            eyre::bail!("file mode does not nest inside file mode");
        }
        // If the supervisor has recorded `output_files` for this completed
        // task, restrict access to those paths. Otherwise (still running, no
        // recorded outputs) allow any path within the workspace root.
        if !task.output_files.is_empty() {
            let normalised = path.trim_start_matches("./");
            let allowed = task
                .output_files
                .iter()
                .any(|f| f == path || f == normalised || f.ends_with(normalised));
            if !allowed {
                eyre::bail!(
                    "path '{}' is not in the task's expected_files; allowed: {:?}",
                    path,
                    task.output_files
                );
            }
        }
        let resolved = resolve_path(&self.workspace_root, path)
            .wrap_err("path must stay inside the workspace")?;
        let text = read_capped(&resolved)?;
        apply_mode(&text, mode)
    }
}

/// Apply an inline read mode (head/tail/grep/line_range) against `text`.
/// `file` mode is rejected here — file is handled at the dispatch site so
/// it can resolve and read the path before recursing once into this helper.
fn apply_mode(text: &str, mode: ReadMode) -> Result<String> {
    match mode {
        ReadMode::Head { lines } => {
            let lines = lines.clamp(1, MAX_LINE_LIMIT);
            Ok(text.lines().take(lines).collect::<Vec<_>>().join("\n"))
        }
        ReadMode::Tail { lines } => {
            let lines = lines.clamp(1, MAX_LINE_LIMIT);
            let all: Vec<&str> = text.lines().collect();
            let start = all.len().saturating_sub(lines);
            Ok(all[start..].join("\n"))
        }
        ReadMode::Grep {
            pattern,
            max_matches,
        } => {
            if pattern.is_empty() {
                eyre::bail!("grep pattern must not be empty");
            }
            let max_matches = max_matches.clamp(1, MAX_GREP_MATCHES);
            let mut hits = Vec::new();
            for (idx, line) in text.lines().enumerate() {
                if line.contains(&pattern) {
                    hits.push(format!("{}:{}", idx + 1, line));
                    if hits.len() >= max_matches {
                        break;
                    }
                }
            }
            if hits.is_empty() {
                Ok(format!("(no matches for {pattern:?})"))
            } else {
                Ok(hits.join("\n"))
            }
        }
        ReadMode::LineRange { start, end } => {
            if start == 0 || end == 0 {
                eyre::bail!("line numbers are 1-indexed");
            }
            if end < start {
                eyre::bail!("end line {end} must be >= start line {start}");
            }
            let span = end - start + 1;
            if span > MAX_LINE_LIMIT {
                eyre::bail!(
                    "line range {start}..={end} spans {span} lines (cap is {MAX_LINE_LIMIT})"
                );
            }
            Ok(text
                .lines()
                .skip(start - 1)
                .take(span)
                .collect::<Vec<_>>()
                .join("\n"))
        }
        ReadMode::File { .. } => {
            eyre::bail!("file mode does not nest inside file mode")
        }
    }
}

/// Read a file on disk, capped at `MAX_READ_BYTES`. Returns an empty string
/// if the file does not exist (the task may not have produced output yet).
fn read_capped(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(eyre::eyre!("failed to open {}: {e}", path.display())),
    };
    let mut buf = Vec::with_capacity(MAX_READ_BYTES.min(64 * 1024));
    let mut limited = file.by_ref().take(MAX_READ_BYTES as u64);
    limited
        .read_to_end(&mut buf)
        .wrap_err_with(|| format!("read {}", path.display()))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Cap the final tool output at `MAX_OUTPUT_BYTES`, byte-safe (UTF-8 boundary).
fn truncate_to_cap(mut s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str("\n…[truncated]");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_supervisor::TaskSupervisor;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn make_tool(
        dir: &Path,
    ) -> (
        Arc<TaskSupervisor>,
        Arc<SubAgentOutputRouter>,
        ReadTaskOutputTool,
    ) {
        let supervisor = Arc::new(TaskSupervisor::new());
        let router = Arc::new(SubAgentOutputRouter::new(dir.join("router")));
        let tool = ReadTaskOutputTool::new(
            supervisor.clone(),
            "session-A",
            Some(router.clone()),
            dir.join("workspace"),
        );
        std::fs::create_dir_all(dir.join("workspace")).unwrap();
        (supervisor, router, tool)
    }

    fn seed_task(
        supervisor: &TaskSupervisor,
        router: &SubAgentOutputRouter,
        tc_id: &str,
        body: &str,
    ) -> String {
        let task_id = supervisor.register("deep_search", tc_id, Some("session-A"));
        supervisor.mark_running(&task_id);
        let session_id = format!("agent:{tc_id}");
        router
            .append(&session_id, &task_id, body.as_bytes())
            .unwrap();
        task_id
    }

    #[tokio::test]
    async fn head_returns_first_n_lines() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let body = "line1\nline2\nline3\nline4\nline5\n";
        let task_id = seed_task(&supervisor, &router, "tc-1", body);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {"kind": "head", "lines": 2}
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output, "line1\nline2");
    }

    #[tokio::test]
    async fn tail_returns_last_n_lines() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let body = "a\nb\nc\nd\ne\n";
        let task_id = seed_task(&supervisor, &router, "tc-2", body);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {"kind": "tail", "lines": 3}
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output, "c\nd\ne");
    }

    #[tokio::test]
    async fn grep_returns_matching_lines_with_line_numbers() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let body = "rust is fast\npython is dynamic\nrust is safe\ngo is concurrent\n";
        let task_id = seed_task(&supervisor, &router, "tc-3", body);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {"kind": "grep", "pattern": "rust", "max_matches": 10}
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("1:rust is fast"));
        assert!(result.output.contains("3:rust is safe"));
        assert!(!result.output.contains("python"));
    }

    #[tokio::test]
    async fn line_range_returns_inclusive_slice() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let body = "1\n2\n3\n4\n5\n";
        let task_id = seed_task(&supervisor, &router, "tc-4", body);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {"kind": "line_range", "start": 2, "end": 4}
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output, "2\n3\n4");
    }

    #[tokio::test]
    async fn file_mode_reads_expected_file_with_inner_mode() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let body = "[router-only stdout]\n";
        let task_id = seed_task(&supervisor, &router, "tc-5", body);

        // Write a file inside the workspace and record it as output_files.
        let file_rel = "research/_report.md";
        let file_abs = dir.path().join("workspace").join(file_rel);
        std::fs::create_dir_all(file_abs.parent().unwrap()).unwrap();
        std::fs::write(&file_abs, "# Report\nLine A\nLine B\nLine C\n").unwrap();
        supervisor.mark_completed(&task_id, vec![file_rel.to_string()]);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {
                    "kind": "file",
                    "path": file_rel,
                    "mode": {"kind": "head", "lines": 2}
                }
            }))
            .await
            .unwrap();
        assert!(result.success, "got: {}", result.output);
        assert_eq!(result.output, "# Report\nLine A");
    }

    #[tokio::test]
    async fn file_mode_rejects_path_outside_expected_files() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let body = "irrelevant\n";
        let task_id = seed_task(&supervisor, &router, "tc-6", body);

        let other_rel = "research/secret.md";
        let other_abs = dir.path().join("workspace").join(other_rel);
        std::fs::create_dir_all(other_abs.parent().unwrap()).unwrap();
        std::fs::write(&other_abs, "shh").unwrap();
        supervisor.mark_completed(&task_id, vec!["research/_report.md".to_string()]);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {
                    "kind": "file",
                    "path": other_rel,
                    "mode": {"kind": "head", "lines": 2}
                }
            }))
            .await;
        assert!(result.is_err() || !result.unwrap().success);
    }

    #[tokio::test]
    async fn unknown_task_handle_fails_cleanly() {
        let dir = tempdir().unwrap();
        let (_supervisor, _router, tool) = make_tool(dir.path());
        let result = tool
            .execute(&json!({"task_handle": "task_nope"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("not found"));
    }

    #[tokio::test]
    async fn cross_session_handle_is_rejected() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        // Register a task under a DIFFERENT session.
        let task_id = supervisor.register("deep_search", "tc-x", Some("session-OTHER"));
        let session_id = "agent:tc-x";
        router.append(session_id, &task_id, b"x\n").unwrap();

        let result = tool
            .execute(&json!({"task_handle": task_id}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("different session"));
    }

    #[tokio::test]
    async fn output_is_capped_at_4kb() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        // Single very long line should still be capped after the mode runs.
        let huge: String = "x".repeat(MAX_OUTPUT_BYTES * 4);
        let task_id = seed_task(&supervisor, &router, "tc-7", &huge);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {"kind": "head", "lines": 1}
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.len() <= MAX_OUTPUT_BYTES + 32);
        assert!(result.output.ends_with("[truncated]"));
    }

    #[test]
    fn nested_file_mode_inside_file_mode_rejected() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let task_id = seed_task(&supervisor, &router, "tc-8", "x\n");

        let res = tool.read_file(
            &supervisor.get_task(&task_id).unwrap(),
            "x.md",
            ReadMode::File {
                path: "y.md".into(),
                mode: Box::new(ReadMode::Head { lines: 1 }),
            },
        );
        assert!(res.is_err());
    }
}
