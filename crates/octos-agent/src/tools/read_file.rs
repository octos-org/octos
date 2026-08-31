//! Read file tool.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{Tool, ToolContext, ToolResult};
use crate::file_state_cache::{CacheEntry, FileStateCache, format_file_unchanged_stub};
use crate::policy::FilesystemScope;

/// Tool for reading file contents.
pub struct ReadFileTool {
    /// Base directory for resolving relative paths.
    base_dir: PathBuf,
    /// Effective filesystem scope.
    filesystem_scope: FilesystemScope,
}

impl ReadFileTool {
    /// Create a new read file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
        }
    }

    /// Set the effective filesystem scope.
    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }
}

#[derive(Debug, Deserialize)]
// #1770: unknown keys are usually a typo of a real parameter; rejecting
// them (with a did-you-mean via `args::parse_tool_args`) lets the model
// self-correct instead of silently dropping its intent.
#[serde(deny_unknown_fields)]
struct ReadFileInput {
    /// #1767: `filePath` is the industry-convention alias.
    #[serde(alias = "filePath")]
    path: String,
    /// `offset` is a 1:1 alias — both mean "first line to read, 1-indexed".
    #[serde(default, alias = "offset")]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    /// #1767: distinct field, NOT an alias of `end_line` — `limit` is a
    /// *count* of lines to read starting at `start_line`, from which the
    /// effective `end_line` is computed. A bare alias would misread
    /// `limit: 100` as "stop at line 100".
    #[serde(default)]
    limit: Option<usize>,
}

/// Resolve the effective `(start_line, end_line)` pair from the three
/// accepted range parameters. `limit` computes `end_line = start_line +
/// limit - 1`; supplying both `end_line` and `limit` is ambiguous and
/// rejected.
fn resolve_line_range(
    start_line: Option<usize>,
    end_line: Option<usize>,
    limit: Option<usize>,
) -> Result<(Option<usize>, Option<usize>), String> {
    match (end_line, limit) {
        (Some(_), Some(_)) => Err("Provide either 'end_line' or 'limit', not both.".to_string()),
        (None, Some(0)) => Err("'limit' must be at least 1.".to_string()),
        (None, Some(count)) => {
            let start = start_line.unwrap_or(1);
            Ok((
                start_line,
                Some(start.saturating_add(count).saturating_sub(1)),
            ))
        }
        (end, None) => Ok((start_line, end)),
    }
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
                    "description": "Path to the file to read (relative to working directory; alias: filePath)"
                },
                "start_line": {
                    "type": "integer",
                    "description": "Optional starting line number (1-indexed; alias: offset)"
                },
                "end_line": {
                    "type": "integer",
                    "description": "Optional ending line number (1-indexed, inclusive)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Optional maximum number of lines to read, starting at start_line (alternative to end_line — do not provide both)"
                }
            },
            "required": ["path"]
        })
    }

    /// `read_file` paginates, so a truncated read has a real next call.
    ///
    /// The advice names `offset`/`limit` explicitly and echoes the range that
    /// was just read, because "output was truncated" alone leaves the model
    /// re-issuing the identical call.
    fn truncation_recovery(
        &self,
        args: &serde_json::Value,
        omitted_bytes: usize,
    ) -> Option<String> {
        let start = args
            .get("offset")
            .or_else(|| args.get("start_line"))
            .and_then(serde_json::Value::as_u64);
        let limit = args.get("limit").and_then(serde_json::Value::as_u64);
        Some(match (start, limit) {
            (Some(start), Some(limit)) => format!(
                "[{omitted_bytes} bytes omitted] This read started at line {start} with limit \
                 {limit}. Continue with offset: {} to read on, or lower limit to read less per \
                 call.",
                start + limit
            ),
            (Some(start), None) => format!(
                "[{omitted_bytes} bytes omitted] This read started at line {start}. Re-read a \
                 bounded range with offset and limit (for example limit: 200) instead of the \
                 whole file."
            ),
            _ => format!(
                "[{omitted_bytes} bytes omitted] Read a bounded range instead: pass offset \
                 (1-indexed start line) and limit (for example offset: 1, limit: 200), then page \
                 forward."
            ),
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
            super::args::parse_tool_args(self.name(), &self.input_schema(), args)?;

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

        // #1767: fold `limit` into an effective end_line up front so every
        // consumer below (range slicing AND the file-state cache key) sees
        // one canonical range.
        let (start_line, end_line) =
            match resolve_line_range(input.start_line, input.end_line, input.limit) {
                Ok(range) => range,
                Err(message) => {
                    return Ok(ToolResult {
                        output: message,
                        success: false,
                        ..Default::default()
                    });
                }
            };

        // Phase 2-C of the SessionScope migration: when the host has
        // threaded a scope through `ToolContext`, use it as the single
        // source of truth for base_dir + path classification. Reads are
        // permitted for `InWorkspace`, `InSharedZone`, and `InGrantedDir`;
        // `OutOfScope` is refused. The shared helper canonicalizes the
        // candidate before classification so ancestor symlinks can't
        // smuggle a path out of the workspace (`O_NOFOLLOW` only
        // protects the final component). When no scope is present we
        // keep the legacy resolver (backward compat for `octos chat`).
        let path = match ctx.session_scope.as_ref() {
            Some(scope) => match super::resolve_path_for_session_scope_read(scope, &input.path) {
                Ok(p) => p,
                Err(reason) => {
                    return Ok(ToolResult {
                        output: format!("{reason}: {}", input.path),
                        success: false,
                        ..Default::default()
                    });
                }
            },
            None => match super::resolve_path_with_scope(
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
            },
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
        let requested_range = user_range(start_line, end_line);
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

        // #2131 part 4: budget-aware reads. An UNBOUNDED read of a file larger
        // than the tool-output budget would be truncated on the way in and then
        // evicted by compaction — forcing the exact re-read loop #2131 targets.
        // Return a range hint instead of accept-then-evict, so the model asks
        // for the slice it needs. A read that already names a range is honored.
        if start_line.is_none() && end_line.is_none() {
            let budget = octos_core::tool_output_limit("read_file");
            if file_size > budget {
                return Ok(ToolResult {
                    output: format!(
                        "{} is {} bytes — larger than the ~{}-byte tool-output budget, so an \
                         unbounded read would be truncated and then evicted from context \
                         (forcing a re-read). Read a bounded range instead: pass start_line and \
                         end_line (e.g. start_line: 1, end_line: 200), or grep for the part you \
                         need first.",
                        input.path, file_size, budget
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        }

        // Read file (O_NOFOLLOW atomically rejects symlinks, no TOCTOU race)
        let content = match super::read_no_follow(&path).await {
            Ok(c) => c,
            Err(e) => return Ok(super::file_io_error(e, &input.path)),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Observe-only (#read-paging probe): record what a FORCED window would
        // have done here. Forcing pages is not a token win — if the model
        // consumes the whole file anyway, more calls cost more, because each
        // re-sends the conversation prefix. It wins only when models stop after
        // page one, and that rate is the number this records. Nothing below
        // changes; the read returns exactly what it always did.
        if super::read_paging_probe::enabled() {
            let bounded = start_line.is_some() || end_line.is_some();
            let max_line_bytes = lines.iter().map(|line| line.len()).max().unwrap_or(0);
            super::read_paging_probe::record_read(
                &path.to_string_lossy(),
                bounded,
                start_line,
                total_lines,
                content.len(),
                max_line_bytes,
            );
        }

        // Apply line range
        let start = start_line.unwrap_or(1).saturating_sub(1);
        let end = end_line.unwrap_or(total_lines).min(total_lines);

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

        // Reject an inverted range (start_line > end_line). Slicing
        // `lines[start..end]` with start > end panics ("slice index starts at
        // N but ends at M"), and that panic was crashing the session actor —
        // taking its in-process sub-agents down with it (mini5 soak). Return a
        // clear, recoverable error instead of slicing.
        if start >= end {
            return Ok(ToolResult {
                output: format!(
                    "Invalid line range: start_line {} is past end_line {}",
                    start + 1,
                    end
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
            output.push_str(&format!("{line_num:>line_num_width$}│ {line}\n"));
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
                let view_range = user_range(start_line, end_line);
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
    use crate::tools::ConcurrencyClass;

    #[test]
    fn read_file_tool_is_safe() {
        // read_file is read-only and side-effect-free — the M8.8 default
        // class is Safe so the executor can parallel-dispatch it with other
        // Safe tools.
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadFileTool::new(dir.path());
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Safe);
    }

    #[tokio::test]
    async fn invalid_args_error_names_each_problem_with_did_you_mean() {
        // #1770: a misspelled parameter must produce a model-facing
        // message that (a) names the missing required parameter, (b)
        // names the unknown parameter, and (c) suggests the correction —
        // so the LLM can self-correct on the next iteration instead of
        // retrying blind.
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadFileTool::new(dir.path());
        let err = match tool
            .execute(&serde_json::json!({"file_path": "a.txt"}))
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("misspelled parameter must fail"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("Invalid arguments for tool 'read_file'"),
            "names the tool: {msg}"
        );
        assert!(
            msg.contains("path") && msg.contains("missing required parameter"),
            "names the missing parameter: {msg}"
        );
        assert!(
            msg.contains("file_path") && msg.contains("unknown parameter"),
            "names the unknown parameter: {msg}"
        );
        assert!(
            msg.contains("did you mean 'path'?"),
            "suggests the correction: {msg}"
        );
        // #1690 contract: argument errors are ToolInputError so a
        // malformed call never cascade-cancels well-formed siblings.
        assert!(
            err.chain()
                .any(|src| src.is::<crate::tools::ToolInputError>()),
            "argument errors must carry the ToolInputError marker"
        );
    }

    #[tokio::test]
    async fn invalid_args_error_reports_type_mismatch() {
        // #1770: wrong-typed values are reported per-parameter with the
        // expected and actual JSON types.
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadFileTool::new(dir.path());
        let err = match tool
            .execute(&serde_json::json!({"path": "a.txt", "start_line": "abc"}))
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("wrong-typed parameter must fail"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("start_line") && msg.contains("expected integer, got string"),
            "reports the type mismatch: {msg}"
        );
    }

    #[tokio::test]
    async fn unknown_extra_parameter_is_rejected_with_suggestion() {
        // #1770: `deny_unknown_fields` — a stray parameter alongside an
        // otherwise valid call is rejected (it is usually a typo of a
        // real parameter, and silently ignoring it hides model bugs).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), b"ok").unwrap();
        let tool = ReadFileTool::new(dir.path());
        let err = match tool
            .execute(&serde_json::json!({"path": "ok.txt", "startline": 1}))
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("unknown parameter must fail"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("startline") && msg.contains("unknown parameter"),
            "names the unknown parameter: {msg}"
        );
        assert!(
            msg.contains("did you mean 'start_line'?"),
            "suggests the near-miss known parameter: {msg}"
        );
    }

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
    async fn read_file_inverted_range_errors_without_panicking() {
        // mini5 soak regression: start_line > end_line used to panic on
        // `lines[start..end]` (start>end), crashing the session actor and
        // orphaning its sub-agents. It must now return a clean error.
        let dir = tempfile::tempdir().unwrap();
        let content = (1..=500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("big.txt"), &content).unwrap();

        let tool = ReadFileTool::new(dir.path());
        // 351 > 100 — the exact shape from the crash ("starts at 350 but ends at 100").
        let result = tool
            .execute(&serde_json::json!({"path": "big.txt", "start_line": 351, "end_line": 100}))
            .await
            .unwrap();

        assert!(!result.success, "inverted range must be a clean failure");
        assert!(
            result.output.contains("Invalid line range") && result.output.contains("351"),
            "should explain the bad range: {}",
            result.output
        );
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

    // -----------------------------------------------------------------------
    // Phase 2-C: SessionScope integration tests for ReadFileTool.
    // -----------------------------------------------------------------------

    use octos_core::SessionScope;

    fn ctx_with_scope(scope: SessionScope) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "read-with-scope".to_string();
        ctx.session_scope = Some(Arc::new(scope));
        ctx
    }

    #[tokio::test]
    async fn read_file_uses_scope_workspace_as_base_dir_for_relative_paths() {
        // When a scope is present, relative paths resolve against
        // `scope.workspace()` regardless of the legacy `base_dir`.
        let scope_dir = tempfile::tempdir().unwrap();
        let legacy_dir = tempfile::tempdir().unwrap();
        std::fs::write(scope_dir.path().join("scoped.txt"), "from scope\n").unwrap();
        std::fs::write(legacy_dir.path().join("scoped.txt"), "from legacy\n").unwrap();

        // Note: legacy_dir is the tool's base_dir, but the scope's
        // workspace is scope_dir — the latter must win.
        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = ReadFileTool::new(legacy_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "scoped.txt"}))
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        assert!(
            result.output.contains("from scope"),
            "expected scope_dir content, got: {}",
            result.output
        );
        assert!(!result.output.contains("from legacy"));
    }

    #[tokio::test]
    async fn read_file_refuses_out_of_scope_path() {
        // An absolute path outside every declared zone classifies as
        // `OutOfScope` and must be refused.
        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("secret.txt");
        std::fs::write(&outside_file, "secret\n").unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = ReadFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": outside_file.to_string_lossy()}),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("outside session scope"),
            "expected scope rejection, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn read_file_allows_in_workspace_path() {
        // `InWorkspace` is the obviously-allowed zone for reads.
        let scope_dir = tempfile::tempdir().unwrap();
        std::fs::write(scope_dir.path().join("ok.txt"), "ok\n").unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = ReadFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "ok.txt"}))
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        assert!(result.output.contains("ok"));
    }

    #[tokio::test]
    async fn read_file_allows_in_shared_zone_path() {
        // Multi-tenant scopes expose shared zones (research/, skills/).
        // READS into those zones are allowed (writes are not — see the
        // write_file tests). The user's intent here is explicit:
        // they're recalling cross-session shared state.
        let data_dir = tempfile::tempdir().unwrap();
        let data = data_dir.path().to_path_buf();
        std::fs::create_dir_all(data.join("research/topic")).unwrap();
        std::fs::create_dir_all(data.join("users/web-1/workspace")).unwrap();
        let shared_file = data.join("research/topic/notes.md");
        std::fs::write(&shared_file, "shared notes\n").unwrap();

        let scope = SessionScope::multi_tenant_with_default_zones(
            data.clone(),
            "dspfac".into(),
            "web-1".into(),
        )
        .unwrap();
        let tool = ReadFileTool::new(scope.workspace());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": shared_file.to_string_lossy()}),
            )
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        assert!(result.output.contains("shared notes"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_refuses_ancestor_symlink_escape() {
        // Per codex review of the Phase 2-C commit: `O_NOFOLLOW` only
        // guards the FINAL path component, and `classify_lexical_path`
        // is explicitly lexical. Without our canonicalization step a
        // path like `<workspace>/link/secret.txt`, where `link` is a
        // symlink pointing outside the workspace, would classify as
        // `InWorkspace` and `read_no_follow` would happily open the
        // file at the symlink's real location.
        use std::os::unix::fs::symlink;

        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        std::fs::write(outside_dir.path().join("secret.txt"), "exfiltrated\n").unwrap();

        // <scope>/link -> <outside>
        let link_path = scope_dir.path().join("link");
        symlink(outside_dir.path(), &link_path).unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = ReadFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "link/secret.txt"}))
            .await
            .unwrap();
        assert!(
            !result.success,
            "ancestor-symlink escape MUST be refused, got: {}",
            result.output
        );
        assert!(
            result.output.contains("outside session scope"),
            "expected scope rejection (canonicalized leaves the workspace), got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn read_file_falls_back_to_legacy_when_no_scope() {
        // No scope on the context — behaviour must match the pre-Phase-2C
        // path (relative resolved against `base_dir`, traversal blocked
        // by the legacy resolver, etc.).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("legacy.txt"), "legacy ok\n").unwrap();

        let tool = ReadFileTool::new(dir.path());
        let ctx = ToolContext::zero();
        assert!(ctx.session_scope.is_none());

        let ok = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "legacy.txt"}))
            .await
            .unwrap();
        assert!(ok.success);
        assert!(ok.output.contains("legacy ok"));

        let bad = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "../escape.txt"}))
            .await
            .unwrap();
        assert!(!bad.success);
        assert!(bad.output.contains("outside working directory"));
    }

    // -----------------------------------------------------------------------
    // #1767: industry-convention parameter aliases.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_accept_file_path_alias_for_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "aliased content\n").unwrap();

        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"filePath": "hello.txt"}))
            .await
            .unwrap();

        assert!(
            result.success,
            "filePath alias must work: {}",
            result.output
        );
        assert!(result.output.contains("aliased content"));
    }

    fn ten_lines_file(dir: &tempfile::TempDir) {
        let content = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("lines.txt"), &content).unwrap();
    }

    #[tokio::test]
    async fn should_accept_offset_alias_for_start_line() {
        let dir = tempfile::tempdir().unwrap();
        ten_lines_file(&dir);

        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "lines.txt", "offset": 3, "end_line": 5}))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("showing lines 3-5 of 10"));
    }

    #[tokio::test]
    async fn should_compute_end_line_from_limit() {
        // limit is a COUNT of lines, not a line number: offset 3 + limit 3
        // reads lines 3..=5 (end_line = start + limit - 1).
        let dir = tempfile::tempdir().unwrap();
        ten_lines_file(&dir);

        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "lines.txt", "offset": 3, "limit": 3}))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(
            result.output.contains("showing lines 3-5 of 10"),
            "limit must be a line count: {}",
            result.output
        );
        assert!(result.output.contains("line 3"));
        assert!(result.output.contains("line 5"));
        assert!(!result.output.contains("line 6"));
    }

    #[tokio::test]
    async fn should_default_start_to_one_when_only_limit_given() {
        let dir = tempfile::tempdir().unwrap();
        ten_lines_file(&dir);

        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "lines.txt", "limit": 2}))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("showing lines 1-2 of 10"));
    }

    #[tokio::test]
    async fn should_reject_when_both_end_line_and_limit_supplied() {
        let dir = tempfile::tempdir().unwrap();
        ten_lines_file(&dir);

        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(
                &serde_json::json!({"path": "lines.txt", "start_line": 2, "end_line": 5, "limit": 2}),
            )
            .await
            .unwrap();

        assert!(!result.success, "must reject ambiguous range");
        assert!(
            result.output.contains("not both"),
            "expected both-supplied rejection, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn should_reject_zero_limit() {
        let dir = tempfile::tempdir().unwrap();
        ten_lines_file(&dir);

        let tool = ReadFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "lines.txt", "limit": 0}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("at least 1"));
    }

    #[test]
    fn resolve_line_range_math() {
        // Pure math checks, including saturation on absurd inputs.
        assert_eq!(resolve_line_range(None, None, None), Ok((None, None)));
        assert_eq!(
            resolve_line_range(Some(3), None, Some(3)),
            Ok((Some(3), Some(5)))
        );
        assert_eq!(resolve_line_range(None, None, Some(2)), Ok((None, Some(2))));
        assert_eq!(
            resolve_line_range(Some(4), Some(9), None),
            Ok((Some(4), Some(9)))
        );
        assert_eq!(
            resolve_line_range(Some(usize::MAX), None, Some(usize::MAX)),
            Ok((Some(usize::MAX), Some(usize::MAX - 1))),
            "absurd inputs saturate instead of overflowing"
        );
        assert!(resolve_line_range(Some(1), Some(2), Some(2)).is_err());
        assert!(resolve_line_range(None, None, Some(0)).is_err());
    }

    #[test]
    fn schema_advertises_canonical_names_only() {
        let tool = ReadFileTool::new("/tmp");
        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("path"));
        assert!(props.contains_key("start_line"));
        assert!(props.contains_key("end_line"));
        assert!(props.contains_key("limit"));
        assert!(!props.contains_key("filePath"));
        assert!(!props.contains_key("offset"));
    }

    #[tokio::test]
    async fn should_hit_cache_when_limit_expresses_same_range_as_end_line() {
        // limit folds into the canonical (start, end) range BEFORE the
        // file-state cache is consulted, so an offset+limit request and a
        // start_line+end_line request for the same lines share one entry.
        let dir = tempfile::tempdir().unwrap();
        ten_lines_file(&dir);

        let tool = ReadFileTool::new(dir.path());
        let cache = Arc::new(FileStateCache::new());
        let ctx = ctx_with_cache(cache.clone());

        let first = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "lines.txt", "offset": 2, "limit": 3}),
            )
            .await
            .unwrap();
        assert!(first.success);
        assert!(!first.output.contains("[FILE_UNCHANGED]"));

        let second = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "lines.txt", "start_line": 2, "end_line": 4}),
            )
            .await
            .unwrap();
        assert!(second.success);
        assert!(
            second.output.contains("[FILE_UNCHANGED]"),
            "same canonical range must hit the cache: {}",
            second.output
        );
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

    /// A truncated read must name the call that continues it.
    ///
    /// Without this the model sees only "N bytes omitted" and its sole
    /// recovery is re-issuing the identical call, which returns the identical
    /// truncation — spending the tokens the cap existed to save.
    #[test]
    fn should_name_the_next_offset_when_a_bounded_read_is_truncated() {
        let tool = ReadFileTool::new(std::path::Path::new("."));
        let advice = tool
            .truncation_recovery(&serde_json::json!({ "offset": 1, "limit": 200 }), 47_000)
            .expect("read_file paginates, so it always has a resume path");
        assert!(advice.contains("47000 bytes omitted"), "{advice}");
        assert!(
            advice.contains("offset: 201"),
            "the advice must name the CONCRETE next call, not just mention offset: {advice}"
        );
    }

    #[test]
    fn should_suggest_bounding_the_read_when_no_range_was_given() {
        let tool = ReadFileTool::new(std::path::Path::new("."));
        let advice = tool
            .truncation_recovery(&serde_json::json!({ "path": "big.txt" }), 12_345)
            .expect("still recoverable: the tool takes offset/limit");
        assert!(advice.contains("offset"), "{advice}");
        assert!(advice.contains("limit"), "{advice}");
    }

    /// #2131 part 4: an UNBOUNDED read of a file bigger than the tool-output
    /// budget returns a range hint (not the body that would be truncated then
    /// evicted); a read that already names a range is honored.
    #[tokio::test]
    async fn oversized_unbounded_read_returns_a_range_hint_not_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let budget = octos_core::tool_output_limit("read_file");
        // Comfortably over the budget, but well under the 10MB hard cap.
        let line = "abcdefghij\n";
        let big = line.repeat(budget / line.len() + 2_000);
        std::fs::write(dir.path().join("big.rs"), &big).unwrap();
        let tool = ReadFileTool::new(dir.path());

        // Unbounded → hint, not the body.
        let r = tool
            .execute(&serde_json::json!({"path": "big.rs"}))
            .await
            .unwrap();
        assert!(
            !r.success,
            "an oversized unbounded read must not dump the body"
        );
        assert!(
            r.output.contains("bounded range"),
            "the hint must tell the model to read a range: {}",
            r.output
        );
        assert!(
            !r.output.contains("abcdefghij"),
            "the body must NOT be returned"
        );

        // A bounded read of the same file is honored (reads the slice).
        let r2 = tool
            .execute(&serde_json::json!({"path": "big.rs", "start_line": 1, "end_line": 3}))
            .await
            .unwrap();
        assert!(r2.success, "a bounded read is honored");
        assert!(
            r2.output.contains("abcdefghij"),
            "the bounded slice returns content"
        );
    }
}
