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
//! - For `file` mode the LLM-supplied path is resolved against
//!   `workspace_root`. Workspace-relative paths go through `resolve_path`
//!   (rejects traversal). Absolute paths are accepted only when (a) they
//!   appear verbatim in `task.output_files` and (b) they normalise to a
//!   path inside the workspace root. The file is also restricted to one
//!   of the task's `output_files`, which the supervisor records once the
//!   task has completed.
//! - All file reads use `O_NOFOLLOW` on Unix (symlink-target reads are
//!   refused atomically; see `read_capped_no_follow`) so a symlink
//!   inside the workspace cannot redirect a read outside it.
//! - All reads are bounded by `MAX_OUTPUT_BYTES` to keep agent context lean.

use std::io::{Read, Seek, SeekFrom};
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
            // Codex P2 (round 1): tail mode for multi-megabyte logs must
            // sample the END of the file, not the first 1 MiB. Pass the
            // mode hint into the read helper so it can seek-from-end.
            inline_mode => {
                let text = self.read_router_text(&task, mode_hint(&inline_mode))?;
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

    fn read_router_text(
        &self,
        task: &crate::task_supervisor::BackgroundTask,
        hint: ReadDirection,
    ) -> Result<String> {
        let path = match self.router_path_for(task) {
            Some(p) => p,
            None => return Ok(String::new()),
        };
        read_capped(&path, hint)
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
        // Codex P1 (round 1): refuse `file` mode until the supervisor has
        // recorded the task's output paths. Without this guard, any task
        // handle with an empty `output_files` (still running, or pre-
        // declaration) grants the LLM read access to any file inside the
        // workspace — far broader than the "dive into one of the task's
        // expected_files" contract documented in the schema. The router
        // modes (head/tail/grep/line_range) still work pre-completion;
        // only `file` is gated.
        if task.output_files.is_empty() {
            eyre::bail!(
                "task '{}' has not yet declared output files; use head/tail/grep on \
                 the captured stdout instead, or wait for the task to complete",
                task.id
            );
        }

        // Codex P1 (round 1) + P2 (round 3): exact normalised-path
        // comparison, supporting both workspace-relative AND absolute
        // `output_files` entries. `check_background_tasks` exposes the
        // exact strings from the supervisor — for tasks routed through
        // the workspace contract those are typically absolute paths
        // under the workspace root — so the LLM may legitimately pass
        // an absolute path back. We accept absolute paths only when:
        //   (a) they appear verbatim in `task.output_files`, AND
        //   (b) they normalise to a path that still lies inside
        //       `workspace_root` (so an absolute `output_files` entry
        //       outside the workspace cannot grant escape).
        let resolved = resolve_handle_path(&self.workspace_root, path)?;
        let allowed_match = task.output_files.iter().any(|f| {
            let candidate = Path::new(f);
            if candidate.is_absolute() {
                normalize_inside_workspace(&self.workspace_root, candidate)
                    .map(|p| p == resolved)
                    .unwrap_or(false)
            } else {
                resolve_path(&self.workspace_root, f)
                    .map(|p| p == resolved)
                    .unwrap_or(false)
            }
        });
        if !allowed_match {
            eyre::bail!(
                "path '{}' is not in the task's expected_files; allowed: {:?}",
                path,
                task.output_files
            );
        }

        // Codex P1 (round 4): the lexical normalisation in
        // `resolve_handle_path` only checks path *spelling*, not
        // filesystem topology. An intermediate directory symlink under
        // the workspace (e.g. `workspace/out -> /etc`) would still
        // satisfy `starts_with(workspace_root)` even though `out/foo`
        // resolves to `/etc/foo` at open time. `O_NOFOLLOW` on the
        // final open only rejects the leaf component, not ancestors.
        // Verify NO ancestor of the resolved path is a symlink before
        // reading. Equivalent to canonicalising and re-checking
        // containment but without following the leaf (which is what
        // O_NOFOLLOW exists for).
        reject_symlinked_ancestors(&self.workspace_root, &resolved)?;

        // Codex P1 (round 1): use the existing O_NOFOLLOW reader so a
        // symlink inside the workspace cannot redirect the read to a file
        // outside the workspace boundary. This matches the safety the
        // built-in `read_file` tool gets.
        //
        // Codex P2 (round 2): for `file` mode with an inner `tail`, the
        // reader must seek to the END of multi-megabyte files; otherwise
        // tail returns the last lines of the first 1 MiB. Mirror the
        // router-side hint logic.
        let text = read_capped_no_follow(&resolved, mode_hint(&mode))?;
        apply_mode(&text, mode)
    }
}

/// Resolve a caller-supplied path against `workspace_root`, accepting
/// either workspace-relative input (the original contract) or an
/// absolute path so long as it lies inside the workspace.
///
/// Codex round 3 P2: required so the LLM can pass back the absolute
/// strings that `check_background_tasks` exposes from the supervisor's
/// `output_files` field (which are often absolute under the workspace).
fn resolve_handle_path(workspace_root: &Path, user_path: &str) -> Result<PathBuf> {
    let p = Path::new(user_path);
    if p.is_absolute() {
        normalize_inside_workspace(workspace_root, p)
            .ok_or_else(|| eyre::eyre!("absolute path '{}' is outside the workspace", user_path))
    } else {
        resolve_path(workspace_root, user_path).wrap_err("path must stay inside the workspace")
    }
}

/// Reject any resolved path whose ancestors (between `workspace_root` and
/// the leaf, exclusive of both) include a symlink.
///
/// Codex round 4 P1: lexical containment is not enough — a symlink under
/// the workspace root that points outside (e.g. `workspace/out -> /etc`)
/// would still satisfy `resolved.starts_with(workspace_root)` while the
/// open-time `O_NOFOLLOW` only rejects the final component. We walk
/// every parent directory of `resolved` up to `workspace_root` and
/// refuse if any segment along the way is itself a symlink.
fn reject_symlinked_ancestors(workspace_root: &Path, resolved: &Path) -> Result<()> {
    // Walk from workspace_root downwards: each ancestor must NOT be a
    // symlink. We deliberately stop one level above the leaf — the leaf
    // is policed by the O_NOFOLLOW open in `read_capped_no_follow`.
    let mut current = workspace_root.to_path_buf();
    let suffix = match resolved.strip_prefix(workspace_root) {
        Ok(s) => s.to_path_buf(),
        Err(_) => {
            // Should not happen: `resolved` was already verified to be
            // inside `workspace_root` by `resolve_handle_path`. Defend
            // anyway.
            eyre::bail!(
                "internal: resolved path {} not inside workspace {}",
                resolved.display(),
                workspace_root.display()
            );
        }
    };
    let comps: Vec<_> = suffix.components().collect();
    if comps.is_empty() {
        return Ok(());
    }
    // Check every intermediate directory. The last component is the
    // file leaf — leave it for O_NOFOLLOW.
    for comp in &comps[..comps.len().saturating_sub(1)] {
        current.push(comp.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                eyre::bail!(
                    "ancestor of '{}' is a symlink ({}); refusing to follow",
                    resolved.display(),
                    current.display()
                );
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Doesn't exist yet — open will fail later; not our concern.
                return Ok(());
            }
            Err(e) => {
                return Err(eyre::eyre!(
                    "failed to stat ancestor {}: {e}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

/// Build a normalised PathBuf for an absolute path, returning `None` if
/// it does not lie inside `workspace_root` after normalisation.
fn normalize_inside_workspace(workspace_root: &Path, abs: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    let mut root = PathBuf::new();
    for comp in workspace_root.components() {
        match comp {
            Component::ParentDir => {
                root.pop();
            }
            Component::CurDir => {}
            other => root.push(other.as_os_str()),
        }
    }
    if out.starts_with(&root) {
        Some(out)
    } else {
        None
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

/// Direction hint for the bounded reader. Tail-style modes need the LAST
/// `MAX_READ_BYTES` of the file rather than the first slice — otherwise a
/// log larger than 1 MiB has its tail clipped to "the last lines of the
/// beginning" and the contract advertised on the schema is broken.
#[derive(Debug, Clone, Copy)]
enum ReadDirection {
    /// Read from the start of the file (head/grep/line_range).
    FromStart,
    /// Read the last `MAX_READ_BYTES` of the file (tail).
    FromEnd,
}

/// Pick the appropriate read direction for a router-side mode.
fn mode_hint(mode: &ReadMode) -> ReadDirection {
    match mode {
        ReadMode::Tail { .. } => ReadDirection::FromEnd,
        _ => ReadDirection::FromStart,
    }
}

/// Read a file from disk, capped at `MAX_READ_BYTES`. Returns an empty
/// string if the file does not exist (the task may not have produced
/// output yet). When `direction` is `FromEnd`, seeks to the end of the
/// file and reads the trailing window — required for `tail` mode against
/// multi-megabyte logs.
fn read_capped(path: &Path, direction: ReadDirection) -> Result<String> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(eyre::eyre!("failed to open {}: {e}", path.display())),
    };
    read_capped_with_direction(file, direction, path)
}

/// O_NOFOLLOW variant for `file` mode: a symlink inside the workspace must
/// not redirect the read to a file outside the workspace boundary.
/// Mirrors the safety the built-in `read_file` tool gets.
fn read_capped_no_follow(path: &Path, direction: ReadDirection) -> Result<String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    {
        if path.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
            eyre::bail!("symlink rejected: {}", path.display());
        }
    }
    let file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(eyre::eyre!("failed to open {}: {e}", path.display())),
    };
    read_capped_with_direction(file, direction, path)
}

fn read_capped_with_direction(
    mut file: std::fs::File,
    direction: ReadDirection,
    path: &Path,
) -> Result<String> {
    let len = file
        .metadata()
        .map(|m| m.len())
        .unwrap_or(MAX_READ_BYTES as u64);
    if matches!(direction, ReadDirection::FromEnd) && len > MAX_READ_BYTES as u64 {
        file.seek(SeekFrom::Start(len - MAX_READ_BYTES as u64))
            .wrap_err_with(|| format!("seek {}", path.display()))?;
    }
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

    // Codex P1 (round 1): file mode must refuse to read until the task has
    // declared its output_files. Without this guard a fresh handle gives the
    // LLM read access to any file inside the workspace.
    #[tokio::test]
    async fn file_mode_rejects_when_task_has_no_recorded_outputs() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let task_id = seed_task(&supervisor, &router, "tc-pre", "still running\n");
        // Do NOT call mark_completed — output_files stays empty.

        let secret_rel = "secret.md";
        let secret_abs = dir.path().join("workspace").join(secret_rel);
        std::fs::write(&secret_abs, "shh").unwrap();

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {
                    "kind": "file",
                    "path": secret_rel,
                    "mode": {"kind": "head", "lines": 1}
                }
            }))
            .await;
        // `execute` may surface this as Err or as success=false; either is
        // a refusal. The important thing is the secret is NOT in the body.
        let body = match result {
            Ok(r) => r.output,
            Err(e) => format!("{e}"),
        };
        assert!(
            !body.contains("shh"),
            "file mode must not return content before output_files declared; got: {body}"
        );
    }

    // Codex P1 (round 1): exact path comparison — `_report.md` must NOT
    // satisfy a whitelist of `research/_report.md`.
    #[tokio::test]
    async fn file_mode_basename_does_not_satisfy_path_whitelist() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let task_id = seed_task(&supervisor, &router, "tc-prefix", "stdout\n");

        // Recorded output: a path under research/.
        let allowed_rel = "research/_report.md";
        let allowed_abs = dir.path().join("workspace").join(allowed_rel);
        std::fs::create_dir_all(allowed_abs.parent().unwrap()).unwrap();
        std::fs::write(&allowed_abs, "# allowed").unwrap();

        // Trap file with a name that COULD match a sloppy `ends_with` check.
        let trap_rel = "_report.md";
        let trap_abs = dir.path().join("workspace").join(trap_rel);
        std::fs::write(&trap_abs, "BAIT - not the report").unwrap();

        supervisor.mark_completed(&task_id, vec![allowed_rel.to_string()]);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {
                    "kind": "file",
                    "path": trap_rel,
                    "mode": {"kind": "head", "lines": 1}
                }
            }))
            .await;
        let body = match result {
            Ok(r) => r.output,
            Err(e) => format!("{e}"),
        };
        assert!(
            !body.contains("BAIT"),
            "exact-path whitelist must reject {trap_rel:?} when only {allowed_rel:?} \
             is recorded; got: {body}"
        );
    }

    // Codex P1 (round 4): a symlinked PARENT directory under the workspace
    // would let `out/passwd` lexically pass containment while resolving to
    // a file outside the workspace at open time. O_NOFOLLOW only checks
    // the leaf — we need to refuse symlinks anywhere along the ancestor
    // chain.
    #[cfg(unix)]
    #[tokio::test]
    async fn file_mode_rejects_symlinked_parent_directory() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let task_id = seed_task(&supervisor, &router, "tc-parent-sym", "stdout\n");

        // Create a target directory outside the workspace with a secret.
        let outside_dir = dir.path().join("etc-fake");
        std::fs::create_dir_all(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("passwd"), "root:x:0:0").unwrap();

        // Symlink workspace/out -> outside_dir.
        let link_inside = dir.path().join("workspace").join("out");
        std::os::unix::fs::symlink(&outside_dir, &link_inside).unwrap();

        // Record an output that traverses the parent symlink. From the
        // tool's POV `out/passwd` looks like a workspace-relative path
        // and the leaf `passwd` is a real file — only the parent is a
        // symlink. Without ancestor checks this would read /etc-fake/passwd.
        let recorded = "out/passwd";
        supervisor.mark_completed(&task_id, vec![recorded.to_string()]);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {
                    "kind": "file",
                    "path": recorded,
                    "mode": {"kind": "head", "lines": 1}
                }
            }))
            .await;
        let body = match result {
            Ok(r) => r.output,
            Err(e) => format!("{e}"),
        };
        assert!(
            !body.contains("root:x:0:0"),
            "parent-directory symlink must not grant read; got: {body}"
        );
    }

    // Codex P1 (round 1): symlink inside the workspace must not redirect
    // reads outside the workspace boundary.
    #[cfg(unix)]
    #[tokio::test]
    async fn file_mode_rejects_symlink_targets() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let task_id = seed_task(&supervisor, &router, "tc-sym", "stdout\n");

        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "secret outside the workspace").unwrap();

        let inside_rel = "linkedin.md";
        let inside_abs = dir.path().join("workspace").join(inside_rel);
        std::os::unix::fs::symlink(&outside, &inside_abs).unwrap();

        supervisor.mark_completed(&task_id, vec![inside_rel.to_string()]);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {
                    "kind": "file",
                    "path": inside_rel,
                    "mode": {"kind": "head", "lines": 1}
                }
            }))
            .await;
        let body = match result {
            Ok(r) => r.output,
            Err(e) => format!("{e}"),
        };
        assert!(
            !body.contains("secret outside"),
            "O_NOFOLLOW must reject symlink target reads; got: {body}"
        );
    }

    // Codex P2 (round 3): file mode must accept absolute paths that
    // appear in `output_files` verbatim, so long as they lie inside
    // the workspace. Workspace-contract spawn tasks often record
    // absolute paths and `check_background_tasks` surfaces those.
    #[tokio::test]
    async fn file_mode_accepts_absolute_path_recorded_in_output_files() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let task_id = seed_task(&supervisor, &router, "tc-abs", "stdout\n");

        let rel = "research/_report.md";
        let abs = dir.path().join("workspace").join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, "# Report\nLine A\nLine B\n").unwrap();

        // Record the ABSOLUTE path — this is what the workspace contract
        // path produces in many real spawn_only tasks.
        supervisor.mark_completed(&task_id, vec![abs.to_string_lossy().into_owned()]);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {
                    "kind": "file",
                    "path": abs.to_string_lossy(),
                    "mode": {"kind": "head", "lines": 1}
                }
            }))
            .await
            .unwrap();
        assert!(result.success, "got: {}", result.output);
        assert_eq!(result.output, "# Report");
    }

    // Codex P2 (round 3): even when an LLM supplies an absolute path,
    // it must still lie inside the workspace root — otherwise an
    // accidentally recorded absolute output_files entry outside the
    // workspace cannot grant escape.
    #[tokio::test]
    async fn file_mode_rejects_absolute_path_outside_workspace() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let task_id = seed_task(&supervisor, &router, "tc-esc", "stdout\n");

        let outside = dir.path().join("escape.md");
        std::fs::write(&outside, "secret outside workspace").unwrap();
        supervisor.mark_completed(&task_id, vec![outside.to_string_lossy().into_owned()]);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {
                    "kind": "file",
                    "path": outside.to_string_lossy(),
                    "mode": {"kind": "head", "lines": 1}
                }
            }))
            .await;
        let body = match result {
            Ok(r) => r.output,
            Err(e) => format!("{e}"),
        };
        assert!(
            !body.contains("secret outside workspace"),
            "absolute paths recorded outside the workspace must not grant access; got: {body}"
        );
    }

    // Codex P2 (round 2): file mode with an inner `tail` must also read
    // from the end of multi-megabyte expected files.
    #[tokio::test]
    async fn file_mode_tail_reads_from_end_for_large_expected_files() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let task_id = seed_task(&supervisor, &router, "tc-big-file", "stdout\n");

        let report_rel = "research/big_report.md";
        let report_abs = dir.path().join("workspace").join(report_rel);
        std::fs::create_dir_all(report_abs.parent().unwrap()).unwrap();

        // Build a > MAX_READ_BYTES file with a unique marker at the end.
        let bulk = "filler-line-pad-pad-pad-pad-pad-pad-pad-pad-pad-pad\n";
        let chunk_count = (MAX_READ_BYTES / bulk.len()) + 100;
        let mut body = bulk.repeat(chunk_count);
        body.push_str("UNIQUE_FILE_TAIL_MARKER\n");
        std::fs::write(&report_abs, &body).unwrap();

        supervisor.mark_completed(&task_id, vec![report_rel.to_string()]);

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {
                    "kind": "file",
                    "path": report_rel,
                    "mode": {"kind": "tail", "lines": 5}
                }
            }))
            .await
            .unwrap();
        assert!(result.success, "got: {}", result.output);
        assert!(
            result.output.contains("UNIQUE_FILE_TAIL_MARKER"),
            "file-mode tail must surface lines from the END of the file; got: {}",
            result.output
        );
    }

    // Codex P2 (round 1): tail mode must read the END of multi-megabyte
    // logs, not the first MAX_READ_BYTES.
    #[tokio::test]
    async fn tail_reads_from_end_for_logs_larger_than_max_read_bytes() {
        let dir = tempdir().unwrap();
        let (supervisor, router, tool) = make_tool(dir.path());
        let task_id = supervisor.register("deep_search", "tc-big", Some("session-A"));
        supervisor.mark_running(&task_id);

        // Build a body well over MAX_READ_BYTES with a unique line near the
        // end. Each line is short so we can fit > 1 MiB while keeping the
        // unique marker in the very last line.
        let bulk = "filler-line-that-takes-up-space-padding-pad-pad\n";
        let session_id = "agent:tc-big";
        // Append bulk in chunks until we're well past MAX_READ_BYTES.
        let chunk_count = (MAX_READ_BYTES / bulk.len()) + 100;
        let bulk_chunk = bulk.repeat(chunk_count);
        router
            .append(session_id, &task_id, bulk_chunk.as_bytes())
            .unwrap();
        let unique_tail = "\nUNIQUE_LAST_LINE_MARKER\n";
        router
            .append(session_id, &task_id, unique_tail.as_bytes())
            .unwrap();

        let result = tool
            .execute(&json!({
                "task_handle": task_id,
                "mode": {"kind": "tail", "lines": 5}
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            result.output.contains("UNIQUE_LAST_LINE_MARKER"),
            "tail mode must surface lines from the END of the log; got: {}",
            result.output
        );
    }
}
