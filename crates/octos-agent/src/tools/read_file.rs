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
    /// Windowed-read enforcement (#1638). `None` = the `OCTOS_READ_WINDOW`
    /// env flag decides (production); `Some` = explicit, for tests — arming
    /// changes output, so tests must not arm process-globally (see
    /// `read_window::armed_from_env`).
    window_enforcement: Option<bool>,
    confined_reads: bool,
}

impl ReadFileTool {
    /// Create a new read file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            window_enforcement: None,
            confined_reads: false,
        }
    }

    /// Set the effective filesystem scope.
    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    /// Host-owned shared sessions may only read through a workspace dirfd.
    pub fn with_confined_reads(mut self) -> Self {
        self.confined_reads = true;
        self
    }

    /// Whether windowed-read enforcement is armed for this instance.
    fn window_armed(&self) -> bool {
        self.window_enforcement
            .unwrap_or_else(super::read_window::armed_from_env)
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
    /// #1638: raw byte mode — 0-indexed byte position to start from. For
    /// content line offsets cannot reach: a single line larger than the
    /// window. Mutually exclusive with the line parameters.
    #[serde(default)]
    byte_offset: Option<usize>,
    /// #1638: maximum bytes to return in raw byte mode (clamped to the
    /// window). Requires `byte_offset`.
    #[serde(default)]
    byte_limit: Option<usize>,
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

    // #1638 R6: the tool spec (description + schema) is serialized into the
    // LLM prompt-cache prefix for EVERY session, so the batch-capable unarmed
    // schema must remain stable across sessions. Both are therefore conditional on the arm:
    // unarmed returns exactly the origin strings; armed adds the windowing
    // contract and the byte-mode parameters. (`window_armed()` reads the
    // per-instance override or `OCTOS_READ_WINDOW`, both stable for a process,
    // so `specs()` sees a consistent answer.)
    fn description(&self) -> &str {
        if self.window_armed() {
            "Read text with line numbers, or attach PNG/JPEG/WEBP images (up to 5 MiB) directly to the model. Large \
             results are truncated to a bounded window and the message names the exact call to \
             continue (offset/limit, or byte_offset for raw byte paging of very long lines) — \
             page forward until no continuation notice remains."
        } else {
            "Read text with line numbers, or attach PNG/JPEG/WEBP images (up to 5 MiB) directly to the model."
        }
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn input_schema(&self) -> serde_json::Value {
        // Single-read parameters are shared by both forms.
        let mut properties = serde_json::json!({
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
        });
        if self.window_armed() {
            let props = properties.as_object_mut().expect("object literal");
            props.insert(
                "byte_offset".to_string(),
                serde_json::json!({
                    "type": "integer",
                    "description": "Raw byte mode: 0-indexed byte position to start reading from. Returns file bytes without line numbers — for single lines too long to page by line offset. Do not combine with start_line/end_line/limit."
                }),
            );
            props.insert(
                "byte_limit".to_string(),
                serde_json::json!({
                    "type": "integer",
                    "description": "Raw byte mode: maximum bytes to return (default and cap: the read window). Requires byte_offset."
                }),
            );
        }
        super::batch_read::schema(
            serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": ["path"]
            }),
            "files",
        )
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
        if args.get("files").is_some() {
            return super::batch_read::execute(self, ctx, args, "files").await;
        }
        let mut result = self.execute_capped_inner(ctx, args).await?;
        // #2193 R4: ONE release-enforced cap over every armed return (success
        // and early errors), so no caller-controlled path or message can slip
        // past the loop's blind head/tail cut. Byte-mode also clamps to the
        // tighter window bound inside; this is the uniform outer backstop.
        if self.window_armed() {
            octos_core::truncate_utf8(
                &mut result.output,
                octos_core::tool_output_limit(self.name()),
                "",
            );
        }
        Ok(result)
    }
}

impl ReadFileTool {
    async fn execute_capped_inner(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let schema = self.input_schema();
        let input: ReadFileInput = super::args::parse_tool_args(
            self.name(),
            &schema["properties"]["files"]["items"],
            args,
        )?;

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

        // #1638 R6: byte mode is part of the ARMED feature. It is resolved
        // once here so the schema/description gating and the execution gating
        // agree.
        let window_armed = self.window_armed();

        // #1638: raw byte mode is a distinct coordinate system — mixing it
        // with line parameters is ambiguous and rejected, like end_line+limit.
        if input.byte_offset.is_some() || input.byte_limit.is_some() {
            // R6: unarmed, byte mode is not advertised in the schema and must
            // not execute. Reject rather than silently fall through to a line
            // read (which would drop the caller's intent). The LLM never hits
            // this — the schema omits the parameters when unarmed — so this
            // only guards manual/legacy callers.
            let message = if !window_armed {
                Some(
                    "byte_offset/byte_limit are only available when windowed reads are enabled \
                     (OCTOS_READ_WINDOW=1)."
                        .to_string(),
                )
            } else if input.start_line.is_some()
                || input.end_line.is_some()
                || input.limit.is_some()
            {
                Some(
                    "Provide either line parameters (start_line/end_line/limit) or byte \
                     parameters (byte_offset/byte_limit), not both."
                        .to_string(),
                )
            } else if input.byte_offset.is_none() {
                Some("'byte_limit' requires 'byte_offset'.".to_string())
            } else if input.byte_limit == Some(0) {
                Some("'byte_limit' must be at least 1.".to_string())
            } else {
                None
            };
            if let Some(message) = message {
                return Ok(ToolResult {
                    output: message,
                    success: false,
                    ..Default::default()
                });
            }
        }

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

        let confined_root = if self.confined_reads {
            let Some(scope) = ctx.session_scope.as_ref() else {
                return Ok(ToolResult { output: "workspace scope unavailable".into(), success: false, ..Default::default() });
            };
            if scope.workspace() != self.base_dir.as_path() || !path.starts_with(scope.workspace()) {
                return Ok(ToolResult { output: "path outside workspace".into(), success: false, ..Default::default() });
            }
            Some(scope.workspace())
        } else { None };

        // Images are a native read result, not a second model request. Resolve
        // permissions and session scope BEFORE inspecting bytes, and bypass the
        // text cache so a repeated image read never returns FILE_UNCHANGED.
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if octos_llm::vision::is_image(&path.to_string_lossy()) || extension == "bmp" {
            if self.confined_reads {
                return Ok(ToolResult { output: "image reads are unavailable in this confined workspace".into(), success: false, ..Default::default() });
            }
            if input.byte_offset.is_some() || input.byte_limit.is_some() {
                return Ok(ToolResult {
                    output: "Image reads attach the whole image; byte paging applies only to text files.".into(),
                    success: false,
                    ..Default::default()
                });
            }
            let ancestor_stop = if let Some(scope) = ctx.session_scope.as_ref() {
                // Shared/granted paths were authorized by the resolver above.
                path.starts_with(scope.workspace())
                    .then_some(scope.workspace())
            } else if self.filesystem_scope == FilesystemScope::Workspace {
                Some(self.base_dir.as_path())
            } else {
                None
            };
            return super::coding_tools::inspect_image_file(
                &path,
                &input.path,
                ancestor_stop,
                self.name(),
            );
        }

        // Reject files larger than 10MB to prevent OOM (output is capped under
        // the tool-output limit anyway, and reading a multi-GB file just to
        // slice a few lines is wasteful).
        const MAX_FILE_BYTES: u64 = 10_000_000;
        let (current_mtime, file_size) = if self.confined_reads { (None, 0) } else { match tokio::fs::metadata(&path).await {
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
        }};

        // M8.4: file-state cache consultation. When the cache is configured
        // and the caller-supplied mtime matches, emit a typed
        // `[FILE_UNCHANGED]` stub rather than re-reading and re-emitting the
        // file body. This reduces token cost by 30-60 % in long sessions.
        // We store the user-supplied range verbatim so the comparison here is
        // exact (without needing to know the file's total line count).
        let requested_range = user_range(start_line, end_line);
        // #1638: a byte-mode request is NOT a line-range request — the cache
        // stores line ranges, so a stored complete entry must never answer a
        // byte request with the [FILE_UNCHANGED] stub (the byte branch below
        // also never stores into the cache).
        if input.byte_offset.is_none()
            && let (Some(cache), Some(mtime)) = (ctx.file_state_cache.as_ref(), current_mtime)
        {
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

        // #1638 R6 (armed-only): raw byte mode. Reached only when armed —
        // unarmed byte params were rejected above. Bypasses the M8.4 cache in
        // BOTH directions (the cache's view ranges are line ranges, so a byte
        // request must never be answered with a line-range [FILE_UNCHANGED]
        // stub, and a byte view must never be stored as one) and bypasses the
        // #2131 refusal (a byte read is bounded by construction).
        if let Some(requested_offset) = input.byte_offset {
            use super::read_window::WINDOW_MAX_BYTES;
            let read = if let Some(root) = confined_root { super::read_confined_with_meta(root, &path).await }
                else { super::read_no_follow_with_meta(&path).await };
            let (content, read_meta) = match read {
                Ok(cm) => cm,
                Err(e) => return Ok(super::file_io_error(e, &input.path)),
            };
            let total = content.len();
            if requested_offset >= total {
                return Ok(ToolResult {
                    output: format!(
                        "byte_offset {requested_offset} is beyond the end of file ({total} bytes)"
                    ),
                    success: false,
                    ..Default::default()
                });
            }
            // Snap the start BACK to a UTF-8 boundary (re-serving at most 3
            // bytes; never leaving a gap), the end back likewise, and
            // guarantee at least one whole character of progress.
            let mut start_b = requested_offset;
            while start_b > 0 && !content.is_char_boundary(start_b) {
                start_b -= 1;
            }
            let want = input
                .byte_limit
                .unwrap_or(WINDOW_MAX_BYTES)
                .min(WINDOW_MAX_BYTES);
            let mut end_b = start_b.saturating_add(want).min(total);
            while end_b > start_b && !content.is_char_boundary(end_b) {
                end_b -= 1;
            }
            if end_b <= start_b {
                end_b = start_b + 1;
                while end_b < total && !content.is_char_boundary(end_b) {
                    end_b += 1;
                }
            }
            let mut output = content[start_b..end_b].to_string();
            if end_b < total {
                output.push_str(&format!(
                    "\n\n[read_file window: bytes {start_b}-{} of {total} (raw byte mode). \
                     Continue with byte_offset: {end_b}.]",
                    end_b - 1
                ));
            }
            // R5: enforce the loop-cap at RUNTIME (not debug_assert, which
            // release builds drop). Sized to fit under the cap by
            // construction, so this never actually cuts; it is the real
            // backstop that keeps the loop's blind head/tail cut from ever
            // mangling the footer in a release build.
            output = clamp_armed_return(output);
            let session = ctx.parent_session_key.clone().unwrap_or_default();
            // T-0380: mirror the loop's routing — a file-content tool only
            // gets the credentials scrub, so `tainted` keeps meaning "the
            // model did not receive these exact bytes" (a 64-hex lockfile
            // checksum is verbatim, not taint).
            let tainted = crate::sanitize::sanitize_credentials(&output) != output;
            super::read_window::record_view(
                &session,
                &path,
                read_meta.epoch,
                start_b,
                end_b,
                total,
                tainted,
                read_meta.transformed,
            );
            return Ok(ToolResult {
                output,
                success: true,
                ..Default::default()
            });
        }

        // #2131 part 4: budget-aware reads. An UNBOUNDED read of a file larger
        // than the tool-output budget would be truncated on the way in and then
        // evicted by compaction — forcing the exact re-read loop #2131 targets.
        // Return a range hint instead of accept-then-evict, so the model asks
        // for the slice it needs. A read that already names a range is honored.
        // ARMED, the refusal is subsumed by the window: page one plus an exact
        // continuation is strictly more useful than a hint with no content.
        if start_line.is_none() && end_line.is_none() && !window_armed {
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
        let read = if let Some(root) = confined_root { super::read_confined_with_meta(root, &path).await }
            else { super::read_no_follow_with_meta(&path).await };
        let (content, read_meta) = match read {
            Ok(cm) => cm,
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

        // #1638 armed window: emit whole formatted lines until either limit
        // would be crossed. Unarmed (or armed and everything fits), this is
        // byte-for-byte the loop that always ran.
        use super::read_window::{WINDOW_MAX_BYTES, WINDOW_MAX_LINES, WindowClamp};
        let mut clamp: Option<WindowClamp> = None;
        let mut included_end = end; // exclusive 0-indexed == last emitted 1-indexed line
        for (idx, line) in lines[start..end].iter().enumerate() {
            if window_armed && idx == WINDOW_MAX_LINES {
                clamp = Some(WindowClamp::Lines);
                included_end = start + idx;
                break;
            }
            let line_num = start + idx + 1;
            let formatted = format!("{line_num:>line_num_width$}│ {line}\n");
            if window_armed && output.len() + formatted.len() > WINDOW_MAX_BYTES {
                if idx == 0 {
                    // The first line of the window alone exceeds the whole
                    // byte budget: line offsets cannot page within a line,
                    // so hand the model the IN-TOOL byte-mode continuation.
                    // Never a shell fallback — shell output is capped at
                    // 30,000 bytes (tool_output_limit("shell")) and the loop
                    // sanitizer redacts exactly what giant lines are made
                    // of, so shell advice is self-defeating end to end.
                    // Nothing is recorded in the view ledger here (the model
                    // received no bytes); the fail-closed write guard treats
                    // that absence as refuse-and-read-first. NOTE: no
                    // caller-controlled text (path spellings are unbounded)
                    // — the model knows the path from its own call.
                    let line_start = line_start_byte_offset(&content, line_num);
                    let mut advice = format!(
                        "[read_file window: line {n} is {len} bytes — larger than the \
                         {WINDOW_MAX_BYTES}-byte window, and lines cannot be split across \
                         line-mode pages. Read it in raw byte mode with byte_offset: \
                         {line_start} (returns bytes without line numbers; follow the \
                         byte_offset each footer names).",
                        n = line_num,
                        len = line.len(),
                    );
                    if line_num < total_lines {
                        advice.push_str(&format!(
                            " Lines after it resume at offset: {}.",
                            line_num + 1
                        ));
                    }
                    advice.push(']');
                    // R5: real runtime clamp (release builds drop
                    // debug_assert). The advice interpolates no unbounded
                    // caller input, so it is already short; the clamp is the
                    // enforced guarantee.
                    return Ok(ToolResult {
                        output: clamp_armed_return(advice),
                        success: true,
                        ..Default::default()
                    });
                }
                clamp = Some(WindowClamp::Bytes);
                included_end = start + idx;
                break;
            }
            output.push_str(&formatted);
        }

        // #2197: the UNARMED cut budget — the loop's per-tool output cap
        // MINUS a footer reserve, never above it. Anything past
        // `tool_output_limit` is head/tail-cut by the execution loop's
        // backstop (#2124), which elides the MIDDLE of the page: the range
        // recorded in the cache below would then claim lines the model never
        // saw (the same lie class as #2197, one level down), and the blind
        // cut would sandwich the advising footer behind an omission marker.
        // The armed window is sized the same way (`WINDOW_MAX_BYTES +
        // FOOTER_RESERVE <= tool_output_limit`, pinned by read_window's
        // tripwire). The reserve is a flat 400 bytes though the footer is
        // ~120, so a formatted output landing in the 400-byte band under the
        // cap is paged rather than served whole — a deliberate trade for one
        // simple, provably-safe budget. Decided before the "(showing lines)"
        // footer below — when the cut fires, that footer would name lines
        // the cut drops, so the advising footer replaces it.
        let output_limit = octos_core::tool_output_limit("read_file");
        let cut_budget = output_limit - super::read_window::FOOTER_RESERVE;
        let unarmed_cut = !window_armed && output.len() > cut_budget;

        match clamp {
            Some(kind) => {
                // The advising footer: which limit fired, the range actually
                // returned, the totals, and the exact next call.
                let shown_from = start + 1;
                let next_offset = included_end + 1;
                let limit_clause = match kind {
                    WindowClamp::Lines => format!("{WINDOW_MAX_LINES}-line limit hit"),
                    WindowClamp::Bytes => format!(
                        "{WINDOW_MAX_BYTES}-byte limit hit; file is {} bytes",
                        content.len()
                    ),
                };
                output.push_str(&format!(
                    "\n[read_file window: showing lines {shown_from}-{included_end} of \
                     {total_lines} — {limit_clause}. Continue with offset: {next_offset}.]"
                ));
                // R5: real runtime clamp (release builds drop debug_assert).
                // The window body is bounded to WINDOW_MAX_BYTES and the
                // footer to well under FOOTER_RESERVE, so this never cuts in
                // practice; it is the enforced backstop.
                output = clamp_armed_return(output);
            }
            None => {
                // Add file info — except when the unarmed cut fires: this
                // footer would name lines the cut drops.
                if !unarmed_cut && (start > 0 || end < total_lines) {
                    output.push_str(&format!(
                        "\n(showing lines {}-{} of {})",
                        start + 1,
                        end,
                        total_lines
                    ));
                }
            }
        }

        // Truncate if too long. UNARMED ONLY: the armed path's advising
        // window above bounds output to WINDOW_MAX_BYTES + a footer, under
        // the execution loop's backstop (#2124), which must never fire on an
        // armed read (a blind head/tail cut would mangle the very footer
        // that names the continuation).
        //
        // #2197: this cut used to be blind, and the M8.4 cache put below
        // recorded the REQUESTED range (`user_range` — `None`, "complete
        // file", for an unbounded read) with the full content hash. The next
        // unchanged-mtime read then short-circuited to `[FILE_UNCHANGED]
        // (full file cached)`, pointing the model at a previous result that
        // was truncated. Cut at a whole-line boundary instead, name the
        // range actually returned and the exact next call, and record THAT
        // range in the cache. The budget keeps the emitted page under the
        // loop's per-tool cap, so what is recorded is what the model
        // actually receives.
        //
        // `cut_shown_to` is the last line actually emitted (1-indexed) when
        // the advising cut fired; `None` when the output was returned whole.
        let mut cut_shown_to: Option<usize> = None;
        // A single formatted line larger than the whole budget cannot be
        // paged by line range (byte mode is armed-only): keep the blind head
        // cut there — the model at least sees the head — but the view is
        // partial with no honest line range, so it must not be cached.
        let mut skip_cache_put = false;
        if unarmed_cut {
            // Every formatted content line ends in '\n' and content lines
            // never contain one, so the last newline inside the budget is a
            // whole-line boundary.
            let mut budget = cut_budget;
            while budget > 0 && !output.is_char_boundary(budget) {
                budget -= 1;
            }
            match output[..budget].rfind('\n') {
                Some(nl) => {
                    output.truncate(nl + 1);
                    let lines_kept = output.bytes().filter(|b| *b == b'\n').count();
                    let shown_to = start + lines_kept;
                    output.push_str(&format!(
                        "\n[read_file truncated: showing lines {}-{shown_to} of {total_lines} — \
                         {cut_budget}-byte output limit hit. Continue with offset: {}.]",
                        start + 1,
                        shown_to + 1
                    ));
                    // The footer is fixed text plus a handful of small
                    // integers — bounded by construction well under
                    // FOOTER_RESERVE — so this never cuts; it is the enforced
                    // guarantee that the loop's blind backstop never fires on
                    // an unarmed read either.
                    octos_core::truncate_utf8(&mut output, output_limit, "");
                    cut_shown_to = Some(shown_to);
                }
                None => {
                    octos_core::truncate_utf8(&mut output, cut_budget, "\n... (content truncated)");
                    skip_cache_put = true;
                }
            }
        }

        // M8.4: record this read in the file-state cache so a later read can
        // short-circuit to the `[FILE_UNCHANGED]` stub. Skip binary blobs —
        // we never want to serve an image/PDF body from the cache.
        //
        // #1638 (b): the recorded view is the view RETURNED, not the view
        // requested. A clamped read stores its actual window, so an unbounded
        // request can never hit a windowed entry and claim
        // `[FILE_UNCHANGED] (full file cached)` against content the model
        // was never shown.
        //
        // #2197: same rule unarmed — when the cut fired, record the EMITTED
        // range, never the requested one (an unbounded request would
        // otherwise record `None`, "complete file", for a truncated view).
        // The cut budget sits under the loop's output cap, so the recorded
        // range is what the model actually receives, end to end.
        let recorded_range = if clamp.is_some() {
            Some(((start + 1) as u64, included_end as u64))
        } else if let Some(shown_to) = cut_shown_to {
            Some(((start + 1) as u64, shown_to as u64))
        } else {
            user_range(start_line, end_line)
        };
        if !skip_cache_put
            && let (Some(cache), Some(mtime)) = (ctx.file_state_cache.as_ref(), current_mtime)
        {
            let can_cache = !FileStateCache::has_binary_extension(&path)
                && FileStateCache::is_text_cacheable(content.as_bytes());
            if can_cache {
                cache.put(CacheEntry::new(
                    path.clone(),
                    mtime,
                    FileStateCache::content_hash(content.as_bytes()),
                    file_size,
                    recorded_range.is_some(),
                    recorded_range,
                ));
            }
        }

        // #1638 (c): feed the view ledger that backs write_file's fail-closed
        // overwrite guard. Armed only — a disarmed read records nothing, so
        // arming later never trusts evidence gathered while off. Coverage is
        // recorded in BYTES (the emitted line range converted to its raw byte
        // span) so line-mode and byte-mode pages stitch in one coordinate
        // system, and the view is TAINTED when the loop sanitizer would alter
        // the output — the model then never received these exact bytes, and a
        // whole-file rewrite from them would substitute redaction
        // placeholders for real content.
        if window_armed {
            let session = ctx.parent_session_key.clone().unwrap_or_default();
            let byte_start = line_start_byte_offset(&content, start + 1);
            let byte_end = if included_end >= total_lines {
                content.len()
            } else {
                line_start_byte_offset(&content, included_end + 1)
            };
            // T-0380: mirror the loop's routing — a file-content tool only
            // gets the credentials scrub, so `tainted` keeps meaning "the
            // model did not receive these exact bytes" (a 64-hex lockfile
            // checksum is verbatim, not taint).
            let tainted = crate::sanitize::sanitize_credentials(&output) != output;
            super::read_window::record_view(
                &session,
                &path,
                read_meta.epoch,
                byte_start,
                byte_end,
                content.len(),
                tainted,
                read_meta.transformed,
            );
        }

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }
}

/// R5: clamp an ARMED `read_file` return as a real runtime operation. Every
/// armed window/byte/advice return is sized to fit within
/// `WINDOW_MAX_BYTES + FOOTER_RESERVE` by construction, so this never cuts in
/// practice — it is the release-build enforcement that a `debug_assert` (which
/// release builds drop) cannot provide. The tripwire test pins
/// `WINDOW_MAX_BYTES + FOOTER_RESERVE <= tool_output_limit("read_file")`, so
/// clamping to that tighter bound also keeps every armed return under the
/// loop's blind head/tail backstop (#2124), which must never mangle a footer.
fn clamp_armed_return(mut output: String) -> String {
    let bound = super::read_window::WINDOW_MAX_BYTES + super::read_window::FOOTER_RESERVE;
    octos_core::truncate_utf8(&mut output, bound, "");
    output
}

/// Byte offset (into the raw content) where 1-indexed `line` starts.
///
/// Counted over `split_inclusive('\n')` so `\r\n` and a missing trailing
/// newline are handled exactly; `line` past EOF returns `content.len()`.
fn line_start_byte_offset(content: &str, line: usize) -> usize {
    content
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum()
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

    // -----------------------------------------------------------------------
    // M8.4 integration tests — file-state cache behaviour in ReadFileTool
    // -----------------------------------------------------------------------
    use std::sync::Arc;

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

    // -----------------------------------------------------------------------
    // #1638: flag-gated windowed reads. Armed via `with_window_enforcement`
    // per instance — never process-globally, because arming CHANGES read_file
    // output and would leak into every unarmed test running in parallel.
    // Every test here asserts on files it created itself (per-path), never on
    // process-global counts (#2077/#2126 lesson).
    // -----------------------------------------------------------------------
}
