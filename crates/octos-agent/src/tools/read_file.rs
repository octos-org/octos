//! Read file tool.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;

use super::{Tool, ToolContext, ToolResult};
use crate::file_state_cache::{CacheEntry, FileStateCache, format_file_unchanged_stub};

/// Tool for reading file contents.
pub struct ReadFileTool {
    /// Base directory for resolving relative paths.
    base_dir: PathBuf,
}

impl ReadFileTool {
    /// Create a new read file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReadFileInput {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns the file content with line numbers."
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
                    "description": "Path to the file to read (relative to working directory)"
                },
                "start_line": {
                    "type": "integer",
                    "description": "Optional starting line number (1-indexed)"
                },
                "end_line": {
                    "type": "integer",
                    "description": "Optional ending line number (1-indexed, inclusive)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        // M8.1: legacy entry point routes through the typed path with a
        // zero-value context so out-of-band callers still exercise the same
        // permission and (post-M8.4) file-state-cache logic.
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let input: ReadFileInput =
            serde_json::from_value(args.clone()).wrap_err("invalid read_file tool input")?;

        // M8.1 permission gate (stub): consult the typed permissions record
        // so the hook is in place before M8.3 wires real allow lists. Today
        // `ToolPermissions::default()` returns allow-all.
        if !ctx.permissions.is_tool_allowed(self.name()) {
            return Ok(ToolResult {
                output: "read_file is not permitted in this context".to_string(),
                success: false,
                ..Default::default()
            });
        }

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

        // Reject files larger than 10MB to prevent OOM (output is capped to 100KB
        // anyway, and reading a multi-GB file just to slice a few lines is wasteful).
        const MAX_FILE_BYTES: u64 = 10_000_000;
        let (current_mtime, file_size) = match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.len() > MAX_FILE_BYTES => {
                return Ok(ToolResult {
                    output: format!(
                        "File too large ({} bytes, max {}). Use start_line/end_line on smaller files.",
                        meta.len(),
                        MAX_FILE_BYTES
                    ),
                    success: false,
                    ..Default::default()
                });
            }
            Ok(meta) => (meta.modified().ok(), meta.len() as usize),
            Err(_) => (None, 0),
        };

        // M8.4: file-state cache consultation. When the cache is configured
        // and the caller-supplied mtime matches, emit a typed
        // `[FILE_UNCHANGED]` stub rather than re-reading and re-emitting the
        // file body. This reduces token cost by 30-60 % in long sessions.
        // We store the user-supplied range verbatim so the comparison here is
        // exact (without needing to know the file's total line count).
        let requested_range = user_range(input.start_line, input.end_line);
        if let (Some(cache), Some(mtime)) = (ctx.file_state_cache.as_ref(), current_mtime) {
            if let Some(entry) = cache.get(&path, mtime) {
                if cache_matches_request(&entry, requested_range) {
                    return Ok(ToolResult {
                        output: format_file_unchanged_stub(&path, entry.view_range),
                        success: true,
                        ..Default::default()
                    });
                }
            }
        }

        // Read file (O_NOFOLLOW atomically rejects symlinks, no TOCTOU race)
        let content = match super::read_no_follow(&path).await {
            Ok(c) => c,
            Err(e) => return Ok(super::file_io_error(e, &input.path)),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Apply line range
        let start = input.start_line.unwrap_or(1).saturating_sub(1);
        let end = input.end_line.unwrap_or(total_lines).min(total_lines);

        if start >= total_lines {
            return Ok(ToolResult {
                output: format!(
                    "Start line {} is beyond file length ({} lines)",
                    start + 1,
                    total_lines
                ),
                success: false,
                ..Default::default()
            });
        }

        // Format with line numbers
        let mut output = String::new();
        let line_num_width = end.to_string().len();

        for (idx, line) in lines[start..end].iter().enumerate() {
            let line_num = start + idx + 1;
            output.push_str(&format!(
                "{:>width$}│ {}\n",
                line_num,
                line,
                width = line_num_width
            ));
        }

        // Add file info
        if start > 0 || end < total_lines {
            output.push_str(&format!(
                "\n(showing lines {}-{} of {})",
                start + 1,
                end,
                total_lines
            ));
        }

        // Truncate if too long
        const MAX_OUTPUT: usize = 100000;
        octos_core::truncate_utf8(&mut output, MAX_OUTPUT, "\n... (content truncated)");

        // M8.4: record this read in the file-state cache so a later read can
        // short-circuit to the `[FILE_UNCHANGED]` stub. Skip binary blobs —
        // we never want to serve an image/PDF body from the cache.
        if let (Some(cache), Some(mtime)) = (ctx.file_state_cache.as_ref(), current_mtime) {
            let can_cache = !FileStateCache::has_binary_extension(&path)
                && FileStateCache::is_text_cacheable(content.as_bytes());
            if can_cache {
                let view_range = user_range(input.start_line, input.end_line);
                cache.put(CacheEntry::new(
                    path.clone(),
                    mtime,
                    FileStateCache::content_hash(content.as_bytes()),
                    file_size,
                    view_range.is_some(),
                    view_range,
                ));
            }
        }

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }
}

/// Encode the user-supplied (start_line, end_line) pair as a cache range.
///
/// Returns `None` when the caller did not provide either bound (meaning "the
/// whole file"). When only one bound is set, the absent side is stored as
/// 0 (for a missing start) or [`u64::MAX`] (for a missing end) so the tuple
/// still compares by identity without needing the file's total-line count.
fn user_range(start: Option<usize>, end: Option<usize>) -> Option<(u64, u64)> {
    if start.is_none() && end.is_none() {
        return None;
    }
    Some((
        start.map(|s| s as u64).unwrap_or(0),
        end.map(|e| e as u64).unwrap_or(u64::MAX),
    ))
}

/// True when a cached entry can satisfy the caller's request without
/// re-reading the file. A full-file cache satisfies any request. A partial
/// cache satisfies a request only if the ranges agree exactly.
fn cache_matches_request(entry: &CacheEntry, requested_range: Option<(u64, u64)>) -> bool {
    match (entry.view_range, requested_range) {
        // Full-file cache covers a full-file request.
        (None, None) => true,
        // A full-file read cannot satisfy a partial request without knowing
        // the file's line count. Be conservative.
        (None, Some(_)) => false,
        // A partial cache cannot satisfy a full request.
        (Some(_), None) => false,
        (Some(cached), Some(requested)) => cached == requested,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_read_file_basic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "line1\nline2\nline3\n").unwrap();

        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "hello.txt"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("line1"));
        assert!(result.output.contains("line2"));
        assert!(result.output.contains("line3"));
    }

    #[tokio::test]
    async fn test_read_file_with_line_range() {
        let dir = tempfile::tempdir().unwrap();
        let content = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("lines.txt"), &content).unwrap();

        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "lines.txt", "start_line": 3, "end_line": 5}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("line 3"));
        assert!(result.output.contains("line 5"));
        assert!(!result.output.contains("line 1"));
        assert!(!result.output.contains("line 6"));
        assert!(result.output.contains("showing lines 3-5 of 10"));
    }

    #[tokio::test]
    async fn test_read_file_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "nope.txt"}))
            .await
            .unwrap();

        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_read_file_traversal_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "../../etc/passwd"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("outside working directory"));
    }

    #[tokio::test]
    async fn test_read_file_start_beyond_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("short.txt"), "one\ntwo\n").unwrap();

        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "short.txt", "start_line": 100}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("beyond file length"));
    }

    #[test]
    fn test_tool_metadata() {
        let tool = ReadFileTool::new("/tmp");
        assert_eq!(tool.name(), "read_file");
        assert!(tool.tags().contains(&"fs"));
    }

    #[tokio::test]
    async fn should_read_via_execute_with_context() {
        // M8.1 migration: `execute_with_context` is the authoritative entry
        // point. Dispatching through it with a populated `ToolContext` must
        // produce the same result as the legacy `execute` path.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "alpha\nbeta\n").unwrap();

        let tool = ReadFileTool::new(dir.path());
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "read-via-ctx".to_string();

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "hello.txt"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("alpha"));
        assert!(result.output.contains("beta"));
    }

    // -----------------------------------------------------------------------
    // M8.4 integration tests — file-state cache behaviour in ReadFileTool
    // -----------------------------------------------------------------------

    use std::sync::Arc;

    fn ctx_with_cache(cache: Arc<FileStateCache>) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "read-with-cache".to_string();
        ctx.file_state_cache = Some(cache);
        ctx
    }

    #[tokio::test]
    async fn should_read_file_tool_return_file_unchanged_when_cache_hit() {
        // First read populates the cache. Second read with unchanged mtime
        // must short-circuit to the [FILE_UNCHANGED] stub.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stable.txt"), "first\nsecond\nthird\n").unwrap();

        let tool = ReadFileTool::new(dir.path());
        let cache = Arc::new(FileStateCache::new());
        let ctx = ctx_with_cache(cache.clone());

        let first = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "stable.txt"}))
            .await
            .unwrap();
        assert!(first.success);
        assert!(first.output.contains("first"));
        assert!(!first.output.contains("[FILE_UNCHANGED]"));
        assert_eq!(cache.len(), 1);

        // Second read: mtime unchanged, must hit the cache and return the stub.
        let second = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "stable.txt"}))
            .await
            .unwrap();
        assert!(second.success);
        assert!(
            second.output.contains("[FILE_UNCHANGED]"),
            "expected stub output, got: {}",
            second.output
        );
        assert!(second.output.contains("stable.txt"));
    }

    #[tokio::test]
    async fn should_read_file_tool_miss_when_file_changed_between_reads() {
        // On most filesystems mtime resolution is coarser than a millisecond.
        // Seed the cache with an explicitly-older mtime so the subsequent
        // rewrite is guaranteed to bump it.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("edits.txt");
        std::fs::write(&file, "v1\n").unwrap();

        let tool = ReadFileTool::new(dir.path());
        let cache = Arc::new(FileStateCache::new());
        let ctx = ctx_with_cache(cache.clone());

        let _ = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "edits.txt"}))
            .await
            .unwrap();
        assert_eq!(cache.len(), 1);

        // Back-date the cached mtime by 5 seconds to simulate a later edit
        // without waiting for wall-clock granularity to change on CI.
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(5);
        cache.put(CacheEntry::new(
            dir.path().join("edits.txt"),
            backdated,
            0xDEAD_BEEF,
            2,
            false,
            None,
        ));

        // Rewriting the file must bust the cache on the next read.
        std::fs::write(&file, "v2_content\n").unwrap();

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "edits.txt"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            !result.output.contains("[FILE_UNCHANGED]"),
            "mtime changed — must NOT hit the cache, got: {}",
            result.output
        );
        assert!(result.output.contains("v2_content"));
    }

    #[tokio::test]
    async fn should_read_file_tool_miss_when_cache_is_none() {
        // Tools with no cache configured must behave identically to the
        // pre-M8.4 path — no stub output, no errors.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n.txt"), "one\n").unwrap();

        let tool = ReadFileTool::new(dir.path());
        let ctx = ToolContext::zero();

        let a = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "n.txt"}))
            .await
            .unwrap();
        let b = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "n.txt"}))
            .await
            .unwrap();
        assert!(a.success && b.success);
        assert!(!a.output.contains("[FILE_UNCHANGED]"));
        assert!(!b.output.contains("[FILE_UNCHANGED]"));
    }

    #[tokio::test]
    async fn should_read_file_tool_not_hit_when_range_differs() {
        // A (1, 5) cache entry cannot satisfy a (3, 7) request.
        let dir = tempfile::tempdir().unwrap();
        let content = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("f.txt"), &content).unwrap();

        let tool = ReadFileTool::new(dir.path());
        let cache = Arc::new(FileStateCache::new());
        let ctx = ctx_with_cache(cache.clone());

        let _ = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "f.txt", "start_line": 1, "end_line": 5}),
            )
            .await
            .unwrap();

        let second = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "f.txt", "start_line": 3, "end_line": 7}),
            )
            .await
            .unwrap();
        assert!(second.success);
        assert!(
            !second.output.contains("[FILE_UNCHANGED]"),
            "different range must not hit cache, got: {}",
            second.output
        );
        assert!(second.output.contains("line 7"));
    }
}
