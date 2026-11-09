//! Diff-based file editing tool using unified diff format.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tracing::warn;

use super::{ConcurrencyClass, Tool, ToolContext, ToolResult};
use crate::policy::{FileAccessMode, FilesystemScope};

/// Tool for editing files via unified diff format with fuzzy matching.
pub struct DiffEditTool {
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    file_access: FileAccessMode,
}

impl DiffEditTool {
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

#[derive(Debug, Deserialize)]
struct DiffEditInput {
    path: String,
    diff: String,
}

#[async_trait]
impl Tool for DiffEditTool {
    fn name(&self) -> &str {
        "diff_edit"
    }

    fn description(&self) -> &str {
        "Apply a unified diff to a file. Supports fuzzy matching (+-3 lines offset). \
         Use standard unified diff format with @@ -start,count +start,count @@ headers."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        // diff_edit writes back to disk after applying the patch — the same
        // race hazard as write_file / edit_file. Serialize. See M8.8.
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit"
                },
                "diff": {
                    "type": "string",
                    "description": "Unified diff to apply (with @@ hunk headers)"
                }
            },
            "required": ["path", "diff"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        // M8.4: legacy entry point routes through the typed path with a
        // zero-value context so out-of-band callers still exercise the same
        // file-state-cache invalidation logic.
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let input: DiffEditInput =
            serde_json::from_value(args.clone()).wrap_err("invalid diff_edit input")?;

        if !self.file_access.allows_write() {
            return Ok(ToolResult {
                output: "diff_edit is not permitted by read-only filesystem access".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // Resolve path (with traversal protection)
        let path = match super::resolve_path_with_scope(
            &self.base_dir,
            &input.path,
            self.filesystem_scope,
        ) {
            Ok(p) => p,
            Err(_) => {
                return Ok(ToolResult {
                    output: format!("Path outside working directory: {}", input.path),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // Never let a file tool touch a `.git` directory: its config/hooks
        // decide what the kernel's own git invocations execute.
        if let Err(reason) = super::refuse_git_internal_path(&path) {
            return Ok(ToolResult {
                output: reason,
                success: false,
                ..Default::default()
            });
        }

        // Read file (O_NOFOLLOW atomically rejects symlinks)
        let (content, metadata) = match super::read_no_follow_with_meta(&path).await {
            Ok(c) => c,
            Err(e) => return Ok(super::file_io_error(e, &input.path)),
        };

        let hunks = match parse_unified_diff(&input.diff) {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Failed to parse diff: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        if hunks.is_empty() {
            return Ok(ToolResult {
                output: "No hunks found in diff".to_string(),
                success: false,
                ..Default::default()
            });
        }

        let previous_content = if metadata.transformed {
            None
        } else {
            super::file_mutation_previous_text(content.clone())
        };
        let new_content = match apply_hunks(&content, &hunks) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Failed to apply diff: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        if let Err(e) = super::write_no_follow(&path, new_content.as_bytes()).await {
            return Ok(super::file_io_error(e, &input.path));
        }

        // #1774: opt-in post-edit formatting. Runs BEFORE cache invalidation
        // and the git snapshot so both observe the final on-disk content.
        // Best-effort by contract — a formatter failure never fails the edit.
        let format_note = if ctx.format_after_edit {
            crate::format::post_edit_format_note(&path, &new_content).await
        } else {
            None
        };

        // M8.4: invalidate any stale cache entry — the file's contents and
        // mtime just changed.
        if let Some(cache) = ctx.file_state_cache.as_ref() {
            cache.invalidate(&path);
        }

        if let Err(error) =
            crate::workspace_git::snapshot_workspace_change(&self.base_dir, &path, "diff_edit")
        {
            warn!(
                path = %input.path,
                error = %error,
                "workspace git snapshot failed after diff_edit"
            );
        }

        Ok(ToolResult {
            output: format!(
                "Applied {} hunk(s) to {}{}",
                hunks.len(),
                input.path,
                format_note.unwrap_or_default()
            ),
            success: true,
            file_modified: Some(path),
            structured_metadata: Some(serde_json::json!({
                "file_created": false,
                "previous_content": previous_content,
            })),
            ..Default::default()
        })
    }
}

// --- Diff parsing ---

struct Hunk {
    old_start: usize,
    lines: Vec<DiffLine>,
}

/// One classified line of a unified-diff hunk body. Shared with the
/// `apply_patch` tool so both editors parse and apply hunk bodies with the
/// same semantics (#1773).
#[derive(Debug)]
pub(crate) enum DiffLine {
    Context(String),
    Remove(String),
    Add(String),
}

/// Context + Remove lines of a hunk body — the block that must match the
/// current file content before the hunk may be applied.
pub(crate) fn pattern_lines(lines: &[DiffLine]) -> Vec<&str> {
    lines
        .iter()
        .filter_map(|l| match l {
            DiffLine::Context(s) | DiffLine::Remove(s) => Some(s.as_str()),
            DiffLine::Add(_) => None,
        })
        .collect()
}

/// Context + Add lines of a hunk body — the block that replaces the matched
/// region.
pub(crate) fn replacement_lines(lines: &[DiffLine]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|l| match l {
            DiffLine::Context(s) | DiffLine::Add(s) => Some(s.clone()),
            DiffLine::Remove(_) => None,
        })
        .collect()
}

fn parse_unified_diff(diff: &str) -> Result<Vec<Hunk>> {
    let mut hunks = Vec::new();
    let mut current_hunk: Option<Hunk> = None;

    for line in diff.lines() {
        if line.starts_with("@@") {
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            if let Some(h) = current_hunk.take() {
                hunks.push(h);
            }
            let old_start = parse_hunk_header(line)?;
            current_hunk = Some(Hunk {
                old_start,
                lines: Vec::new(),
            });
        } else if let Some(hunk) = current_hunk.as_mut() {
            if let Some(rest) = line.strip_prefix('-') {
                hunk.lines.push(DiffLine::Remove(rest.to_string()));
            } else if let Some(rest) = line.strip_prefix('+') {
                hunk.lines.push(DiffLine::Add(rest.to_string()));
            } else if let Some(rest) = line.strip_prefix(' ') {
                hunk.lines.push(DiffLine::Context(rest.to_string()));
            } else if !line.starts_with("---")
                && !line.starts_with("+++")
                && !line.starts_with("diff ")
                && !line.starts_with("index ")
            {
                // Treat unmarked lines as context
                hunk.lines.push(DiffLine::Context(line.to_string()));
            }
        }
        // Lines before any hunk header (e.g., --- a/file, +++ b/file) are ignored
    }

    if let Some(h) = current_hunk {
        hunks.push(h);
    }

    Ok(hunks)
}

fn parse_hunk_header(line: &str) -> Result<usize> {
    // @@ -old_start[,old_count] +new_start[,new_count] @@
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 3 {
        eyre::bail!("invalid hunk header: {}", line);
    }
    let old_part = parts[1]; // e.g., "-10,5"
    let start_str = old_part
        .trim_start_matches('-')
        .split(',')
        .next()
        .unwrap_or("1");
    let start: usize = start_str
        .parse()
        .wrap_err_with(|| format!("invalid line number in hunk header: {line}"))?;
    Ok(start)
}

// --- Hunk application with fuzzy matching ---

const FUZZY_RANGE: i64 = 3;

fn apply_hunks(content: &str, hunks: &[Hunk]) -> Result<String> {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();

    // Apply hunks in reverse order so line numbers stay valid
    let mut sorted_hunks: Vec<(usize, &Hunk)> = hunks.iter().enumerate().collect();
    sorted_hunks.sort_by_key(|entry| std::cmp::Reverse(entry.1.old_start));

    // Check for overlapping hunks (sorted descending by old_start)
    for window in sorted_hunks.windows(2) {
        let (_, later_hunk) = window[0]; // higher line number
        let (_, earlier_hunk) = window[1]; // lower line number
        let earlier_end = earlier_hunk.old_start + pattern_lines(&earlier_hunk.lines).len();
        if earlier_end > later_hunk.old_start {
            eyre::bail!(
                "overlapping hunks at lines {} and {}",
                earlier_hunk.old_start,
                later_hunk.old_start
            );
        }
    }

    for (idx, hunk) in sorted_hunks {
        let context_lines = pattern_lines(&hunk.lines);

        if context_lines.is_empty() {
            eyre::bail!("hunk {} has no context or remove lines", idx + 1);
        }

        // Try exact position first, then fuzzy search
        let target = hunk.old_start.saturating_sub(1); // 1-indexed to 0-indexed
        let match_pos = find_match(&lines, &context_lines, target)?;

        // Apply the hunk at match_pos: replace the matched pattern block with
        // the replacement block.
        let remove_count = context_lines.len();
        let new_lines = replacement_lines(&hunk.lines);

        // Replace the matched region
        let end = (match_pos + remove_count).min(lines.len());
        lines.splice(match_pos..end, new_lines);
    }

    // Preserve trailing newline if original had one
    let mut result = lines.join("\n");
    if content.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

fn find_match(lines: &[String], pattern: &[&str], target: usize) -> Result<usize> {
    // Try exact position
    if matches_at(lines, pattern, target) {
        return Ok(target);
    }

    // Fuzzy search around target
    for offset in 1..=FUZZY_RANGE {
        let above = target as i64 - offset;
        let below = target as i64 + offset;

        if above >= 0 && matches_at(lines, pattern, above as usize) {
            return Ok(above as usize);
        }
        if (below as usize) < lines.len() && matches_at(lines, pattern, below as usize) {
            return Ok(below as usize);
        }
    }

    eyre::bail!(
        "could not find matching context at line {} (+-{} lines). Expected: {:?}",
        target + 1,
        FUZZY_RANGE,
        &pattern[..pattern.len().min(3)]
    )
}

/// Whether `pattern` matches `lines` starting at `start`, comparing with
/// trailing whitespace ignored. Shared with the `apply_patch` tool's
/// sequential hunk matcher (#1773).
pub(crate) fn matches_at(lines: &[String], pattern: &[&str], start: usize) -> bool {
    if start + pattern.len() > lines.len() {
        return false;
    }
    pattern
        .iter()
        .enumerate()
        .all(|(i, p)| lines[start + i].trim_end() == p.trim_end())
}
