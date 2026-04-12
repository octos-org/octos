//! Diff-based file editing tool using unified diff format.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tracing::warn;

use super::{Tool, ToolResult};

/// Tool for editing files via unified diff format with fuzzy matching.
pub struct DiffEditTool {
    base_dir: PathBuf,
}

impl DiffEditTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
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
        let input: DiffEditInput =
            serde_json::from_value(args.clone()).wrap_err("invalid diff_edit input")?;

        // Resolve path (with traversal protection)
        let path = match super::resolve_path(&self.base_dir, &input.path) {
            Ok(p) => p,
            Err(_) => {
                return Ok(ToolResult {
                    output: format!("Path outside working directory: {}", input.path),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // Read file (O_NOFOLLOW atomically rejects symlinks)
        let content = match super::read_no_follow(&path).await {
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
            output: format!("Applied {} hunk(s) to {}", hunks.len(), input.path),
            success: true,
            file_modified: Some(path),
            ..Default::default()
        })
    }
}

// --- Diff parsing ---

struct Hunk {
    old_start: usize,
    lines: Vec<DiffLine>,
}

enum DiffLine {
    Context(String),
    Remove(String),
    Add(String),
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
        .wrap_err_with(|| format!("invalid line number in hunk header: {}", line))?;
    Ok(start)
}

// --- Hunk application with fuzzy matching ---

const FUZZY_RANGE: i64 = 3;

fn apply_hunks(content: &str, hunks: &[Hunk]) -> Result<String> {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();

    // Apply hunks in reverse order so line numbers stay valid
    let mut sorted_hunks: Vec<(usize, &Hunk)> = hunks.iter().enumerate().collect();
    sorted_hunks.sort_by(|a, b| b.1.old_start.cmp(&a.1.old_start));

    // Check for overlapping hunks (sorted descending by old_start)
    for window in sorted_hunks.windows(2) {
        let (_, later_hunk) = window[0]; // higher line number
        let (_, earlier_hunk) = window[1]; // lower line number
        let earlier_end = earlier_hunk.old_start
            + earlier_hunk
                .lines
                .iter()
                .filter(|l| matches!(l, DiffLine::Context(_) | DiffLine::Remove(_)))
                .count();
        if earlier_end > later_hunk.old_start {
            eyre::bail!(
                "overlapping hunks at lines {} and {}",
                earlier_hunk.old_start,
                later_hunk.old_start
            );
        }
    }

    for (idx, hunk) in sorted_hunks {
        let context_lines: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                DiffLine::Context(s) => Some(s.as_str()),
                DiffLine::Remove(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();

        if context_lines.is_empty() {
            eyre::bail!("hunk {} has no context or remove lines", idx + 1);
        }

        // Try exact position first, then fuzzy search
        let target = hunk.old_start.saturating_sub(1); // 1-indexed to 0-indexed
        let match_pos = find_match(&lines, &context_lines, target)?;

        // Apply the hunk at match_pos
        let remove_count = hunk
            .lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Context(_) | DiffLine::Remove(_)))
            .count();

        let new_lines: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                DiffLine::Context(s) | DiffLine::Add(s) => Some(s.clone()),
                DiffLine::Remove(_) => None,
            })
            .collect();

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

fn matches_at(lines: &[String], pattern: &[&str], start: usize) -> bool {
    if start + pattern.len() > lines.len() {
        return false;
    }
    pattern
        .iter()
        .enumerate()
        .all(|(i, p)| lines[start + i].trim_end() == p.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_diff() {
        let diff = "@@ -1,3 +1,3 @@\n line1\n-line2\n+line2_modified\n line3";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].lines.len(), 4); // context + remove + add + context
    }

    #[test]
    fn test_apply_simple_replacement() {
        let content = "line1\nline2\nline3\n";
        let diff = "@@ -1,3 +1,3 @@\n line1\n-line2\n+line2_new\n line3\n";
        let hunks = parse_unified_diff(diff).unwrap();
        let result = apply_hunks(content, &hunks).unwrap();
        assert_eq!(result, "line1\nline2_new\nline3\n");
    }

    #[test]
    fn test_apply_insertion() {
        let content = "a\nb\n";
        let diff = "@@ -1,2 +1,3 @@\n a\n+inserted\n b\n";
        let hunks = parse_unified_diff(diff).unwrap();
        let result = apply_hunks(content, &hunks).unwrap();
        assert_eq!(result, "a\ninserted\nb\n");
    }

    #[test]
    fn test_apply_deletion() {
        let content = "a\ndelete_me\nb\n";
        let diff = "@@ -1,3 +1,2 @@\n a\n-delete_me\n b\n";
        let hunks = parse_unified_diff(diff).unwrap();
        let result = apply_hunks(content, &hunks).unwrap();
        assert_eq!(result, "a\nb\n");
    }

    #[test]
    fn test_fuzzy_match_offset() {
        // Content has an extra line at the top, so line numbers are off by 1
        let content = "extra\nline1\nline2\nline3\n";
        // Diff says line 1, but actual match is at line 2
        let diff = "@@ -1,3 +1,3 @@\n line1\n-line2\n+line2_fuzzy\n line3\n";
        let hunks = parse_unified_diff(diff).unwrap();
        let result = apply_hunks(content, &hunks).unwrap();
        assert_eq!(result, "extra\nline1\nline2_fuzzy\nline3\n");
    }

    #[test]
    fn test_multiple_hunks() {
        let content = "a\nb\nc\nd\ne\nf\n";
        let diff = "@@ -1,2 +1,2 @@\n-a\n+A\n b\n@@ -5,2 +5,2 @@\n-e\n+E\n f\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        let result = apply_hunks(content, &hunks).unwrap();
        assert_eq!(result, "A\nb\nc\nd\nE\nf\n");
    }

    #[test]
    fn test_overlapping_hunks_rejected() {
        let content = "a\nb\nc\nd\ne\n";
        // Two hunks that overlap: first covers lines 1-3, second starts at line 2
        let diff = "@@ -1,3 +1,3 @@\n-a\n+A\n b\n c\n@@ -2,3 +2,3 @@\n-b\n+B\n c\n d\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        let result = apply_hunks(content, &hunks);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("overlapping hunks"),
            "expected overlapping hunks error, got: {err_msg}"
        );
    }
}
