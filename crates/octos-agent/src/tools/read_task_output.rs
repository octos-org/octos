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
//!   `workspace_root` through
//!   [`octos_core::file_handle::resolve_tool_path`]. Workspace-relative
//!   paths reject traversal, absolute paths must lie inside the
//!   workspace root, and any resolution outside the
//!   [`octos_core::file_handle::ToolPathScope::Workspace`] scope is
//!   refused — this tool does not surface uploads or profile files.
//!   The file is also restricted to one of the task's `output_files`,
//!   which the supervisor records once the task has completed.
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

use super::{Tool, ToolResult};

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
                let mut text = self.read_router_text(&task, mode_hint(&inline_mode))?;
                // Black-hole fix: a background `spawn` child's transcript
                // never flows through the SubAgentOutputRouter (only
                // spawn_only tools append to it), so for such tasks the
                // router file is absent and this tool silently returned an
                // empty string — models concluded the child's result "was
                // lost" and re-did (or overwrote) its work. Fall back to the
                // supervisor-recorded final output so head/tail/grep operate
                // on the child's actual result.
                if text.is_empty() {
                    if let Some(final_output) = task.final_output.as_deref() {
                        text = final_output.to_string();
                    }
                }
                if text.is_empty() {
                    // Say WHY there is nothing rather than returning "".
                    format!(
                        "(no captured output for task '{}' yet; status={} — a \
                         running task's result is recorded when it finishes)",
                        task.id,
                        task.status.as_str()
                    )
                } else {
                    apply_mode(&text, inline_mode)?
                }
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
        // Resolve every `output_files` entry through the same routine
        // we use for the caller-supplied path, then compare normalised
        // forms. This keeps the absolute-vs-relative semantics
        // identical on both sides of the whitelist check.
        let allowed_match = task.output_files.iter().any(|f| {
            resolve_handle_path(&self.workspace_root, f)
                .map(|p| p == resolved)
                .unwrap_or(false)
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
///
/// Routed through the unified
/// [`octos_core::file_handle::resolve_tool_path`] resolver since the
/// unified table already encodes "absolute inside workspace OR
/// workspace-relative" semantics. Only the [`ToolPathScope::Workspace`]
/// scope is permitted here — upload-tmpdir / profile-root scopes would
/// grant the LLM read access to paths outside the per-task
/// `output_files` whitelist, which is the whole point of this tool's
/// gating.
fn resolve_handle_path(workspace_root: &Path, user_path: &str) -> Result<PathBuf> {
    use octos_core::file_handle::{ToolPathError, ToolPathScope, resolve_tool_path};
    match resolve_tool_path(workspace_root, None, user_path) {
        Ok(resolved) => {
            if resolved.scope == ToolPathScope::Workspace {
                Ok(resolved.absolute)
            } else {
                eyre::bail!(
                    "path '{}' resolved outside the workspace ({:?}); only workspace paths are permitted",
                    user_path,
                    resolved.scope
                )
            }
        }
        Err(ToolPathError::Traversal) => {
            eyre::bail!("path must stay inside the workspace: {}", user_path)
        }
        Err(ToolPathError::OutsideAllowedRoots) => {
            eyre::bail!("absolute path '{}' is outside the workspace", user_path)
        }
        Err(ToolPathError::DecodeFailed) => {
            eyre::bail!("path must stay inside the workspace: {}", user_path)
        }
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
    //
    // CRITICAL: walk the ORIGINAL (lexical) path components, not the
    // canonical form. If we canonicalised `resolved` here we'd rewrite
    // a `workspace/out/file` chain — where `out` is a symlink to some
    // other workspace dir — into the symlink target before the loop
    // ran, so the loop would stat the *target's* ancestors and miss
    // the symlink it was supposed to refuse. The actual `read_capped_
    // no_follow` then still opens the original path and follows the
    // parent symlink. Codex review round 3 P2 (2026-05-13) pinned
    // this exact escape.
    //
    // Canonicalise the ROOT only — that's needed because on macOS the
    // supplied `workspace_root` is typically the un-prefixed
    // `/var/folders/...` form while `resolved` for absolute inputs may
    // come back in the `/private/var/folders/...` firmlink form. We
    // build a candidate `(canonical_root, lexical_root)` pair and try
    // strip_prefix against both so absolute and workspace-relative
    // resolutions both succeed.
    let canonical_root =
        std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    let lexical_root = workspace_root.to_path_buf();
    let (mut current, suffix) = if let Ok(s) = resolved.strip_prefix(&lexical_root) {
        (lexical_root, s.to_path_buf())
    } else if let Ok(s) = resolved.strip_prefix(&canonical_root) {
        (canonical_root, s.to_path_buf())
    } else {
        // Should not happen: `resolved` was already verified to be
        // inside `workspace_root` by `resolve_handle_path`. Defend
        // anyway.
        eyre::bail!(
            "internal: resolved path {} not inside workspace {}",
            resolved.display(),
            workspace_root.display()
        );
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

// Note: the previous `normalize_inside_workspace` helper retired with
// the migration to `octos_core::file_handle::resolve_tool_path`. The
// unified resolver does the absolute-vs-workspace containment check
// (with macOS firmlink collapsing) so this duplicate is no longer
// needed.

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
