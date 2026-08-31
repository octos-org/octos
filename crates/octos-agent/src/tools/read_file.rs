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
}

impl ReadFileTool {
    /// Create a new read file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            window_enforcement: None,
        }
    }

    /// Set the effective filesystem scope.
    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    /// Test-only arming override for windowed-read enforcement, so no test
    /// has to mutate the process environment (`set_var` is `unsafe` under
    /// edition 2024 and this workspace denies unsafe) or leak armed
    /// behaviour into parallel unarmed tests.
    #[cfg(test)]
    pub(crate) fn with_window_enforcement(mut self, armed: bool) -> Self {
        self.window_enforcement = Some(armed);
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
    // LLM prompt-cache prefix for EVERY session, so it must be byte-identical
    // to origin/main when the flag is OFF, or arming one process would bust
    // the prefix for all of them. Both are therefore conditional on the arm:
    // unarmed returns exactly the origin strings; armed adds the windowing
    // contract and the byte-mode parameters. (`window_armed()` reads the
    // per-instance override or `OCTOS_READ_WINDOW`, both stable for a process,
    // so `specs()` sees a consistent answer.)
    fn description(&self) -> &str {
        if self.window_armed() {
            "Read the contents of a file. Returns the file content with line numbers. Large \
             results are truncated to a bounded window and the message names the exact call to \
             continue (offset/limit, or byte_offset for raw byte paging of very long lines) — \
             page forward until no continuation notice remains."
        } else {
            "Read the contents of a file. Returns the file content with line numbers."
        }
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn input_schema(&self) -> serde_json::Value {
        // Origin/main properties — MUST stay byte-identical when unarmed.
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
        serde_json::json!({
            "type": "object",
            "properties": properties,
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
            let content = match super::read_no_follow(&path).await {
                Ok(c) => c,
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
            let epoch = current_mtime.map(|mtime| super::read_window::ViewEpoch {
                mtime,
                size: file_size as u64,
            });
            let tainted = crate::sanitize::sanitize_tool_output(&output) != output;
            super::read_window::record_view(&session, &path, epoch, start_b, end_b, total, tainted);
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
                // Add file info
                if start > 0 || end < total_lines {
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
        // window above bounds output to WINDOW_MAX_BYTES + a footer — under
        // both this blind cut and the execution loop's 50,000-byte backstop
        // (#2124), which must never fire on an armed read (a blind head/tail
        // cut would mangle the very footer that names the continuation).
        if !window_armed {
            const MAX_OUTPUT: usize = 100000;
            octos_core::truncate_utf8(&mut output, MAX_OUTPUT, "\n... (content truncated)");
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
        let recorded_range = if clamp.is_some() {
            Some(((start + 1) as u64, included_end as u64))
        } else {
            user_range(start_line, end_line)
        };
        if let (Some(cache), Some(mtime)) = (ctx.file_state_cache.as_ref(), current_mtime) {
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
            let epoch = current_mtime.map(|mtime| super::read_window::ViewEpoch {
                mtime,
                size: file_size as u64,
            });
            let byte_start = line_start_byte_offset(&content, start + 1);
            let byte_end = if included_end >= total_lines {
                content.len()
            } else {
                line_start_byte_offset(&content, included_end + 1)
            };
            let tainted = crate::sanitize::sanitize_tool_output(&output) != output;
            super::read_window::record_view(
                &session,
                &path,
                epoch,
                byte_start,
                byte_end,
                content.len(),
                tainted,
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

    // -----------------------------------------------------------------------
    // #1638: flag-gated windowed reads. Armed via `with_window_enforcement`
    // per instance — never process-globally, because arming CHANGES read_file
    // output and would leak into every unarmed test running in parallel.
    // Every test here asserts on files it created itself (per-path), never on
    // process-global counts (#2077/#2126 lesson).
    // -----------------------------------------------------------------------

    /// R6: the UNARMED tool must be byte-for-byte the origin/main tool at the
    /// WIRE — same name, description, and input schema — because every enabled
    /// tool's ToolSpec is serialized into the LLM prompt-cache prefix
    /// (registry.rs `specs()`). A changed unarmed spec would invalidate that
    /// prefix for every session on the planet, armed or not, defeating the
    /// "flag-gated, zero blast radius" premise. This golden is the exact
    /// origin/main ToolSpec JSON; only the ARMED tool may differ from it.
    fn read_file_origin_toolspec() -> serde_json::Value {
        serde_json::json!({
            "name": "read_file",
            "description": "Read the contents of a file. Returns the file content with line numbers.",
            "input_schema": {
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
            }
        })
    }

    fn read_file_toolspec(tool: &ReadFileTool) -> serde_json::Value {
        serde_json::json!({
            "name": tool.name(),
            "description": tool.description(),
            "input_schema": tool.input_schema(),
        })
    }

    #[test]
    fn unarmed_read_file_toolspec_is_byte_identical_to_origin_main() {
        let tool = ReadFileTool::new("/tmp").with_window_enforcement(false);
        let spec = read_file_toolspec(&tool);
        assert_eq!(
            spec,
            read_file_origin_toolspec(),
            "the UNARMED read_file ToolSpec must equal origin/main exactly — no \
             byte_offset/byte_limit in the schema, no windowing sentence in the \
             description — or the prompt-cache prefix changes for every session"
        );
        // The wire is the serialized string; pin it too (serde_json sorts
        // keys, so this is deterministic).
        assert_eq!(
            serde_json::to_string(&spec).unwrap(),
            serde_json::to_string(&read_file_origin_toolspec()).unwrap(),
            "serialized unarmed spec must match origin byte-for-byte"
        );
    }

    #[test]
    fn armed_read_file_toolspec_advertises_byte_mode() {
        // The armed spec is ALLOWED to differ — byte mode is part of the
        // armed feature — and it must actually carry the byte parameters.
        let tool = ReadFileTool::new("/tmp").with_window_enforcement(true);
        let spec = read_file_toolspec(&tool);
        assert_ne!(
            spec,
            read_file_origin_toolspec(),
            "the armed spec differs from origin by design"
        );
        let props = spec["input_schema"]["properties"].as_object().unwrap();
        assert!(
            props.contains_key("byte_offset") && props.contains_key("byte_limit"),
            "armed schema advertises byte mode: {props:?}"
        );
    }

    #[tokio::test]
    async fn unarmed_byte_params_are_rejected_not_silently_ignored() {
        // byte mode is an armed-only capability. Unarmed, the schema does not
        // advertise it, so the model never sends it; a manual caller that
        // does must get a clear error, never a silent fall-through to a line
        // read (which would drop its intent).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "abcdef").unwrap();
        let tool = ReadFileTool::new(dir.path()); // unarmed
        let r = tool
            .execute(&serde_json::json!({"path": "f.txt", "byte_offset": 0}))
            .await
            .unwrap();
        assert!(
            !r.success && r.output.contains("byte_offset"),
            "unarmed byte_offset must be a clean rejection: {}",
            r.output
        );
    }

    /// 1500 lines, 100 bytes of content each (distinct `row NNNNNN` prefixes),
    /// 151,500 content bytes total. With a 4-digit gutter each formatted line
    /// is 109 bytes, so the 49,152-byte window holds exactly 450 of them:
    /// 450 x 109 = 49,050 fits, 451 would not.
    fn wide_rows_file(dir: &tempfile::TempDir, name: &str) {
        let content = (1..=1500)
            .map(|i| format!("row {i:06}{}", "z".repeat(90)))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join(name), &content).unwrap();
    }

    #[tokio::test]
    async fn should_window_an_unbounded_read_of_a_big_file_when_armed() {
        let dir = tempfile::tempdir().unwrap();
        wide_rows_file(&dir, "big_armed.txt");
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);

        let r = tool
            .execute(&serde_json::json!({"path": "big_armed.txt"}))
            .await
            .unwrap();

        assert!(
            r.success,
            "armed, the read returns page one instead of the unarmed refusal: {}",
            r.output
        );
        assert!(r.output.contains("row 000001"), "page one starts at line 1");
        assert!(
            !r.output.contains("row 000451"),
            "the byte limit stops the window at line 450"
        );
        assert!(
            r.output.contains("showing lines 1-450 of 1500"),
            "the footer names the actual range returned and the total: {}",
            r.output
        );
        assert!(
            r.output.contains("-byte limit"),
            "the footer names WHICH limit fired (bytes, not lines): {}",
            r.output
        );
        assert!(
            r.output.contains("offset: 451"),
            "the footer names the exact next call: {}",
            r.output
        );
        assert!(
            r.output.len() <= octos_core::tool_output_limit("read_file"),
            "the tool's own advising cut must keep the loop's blind backstop from \
             ever firing on an armed read: {} bytes",
            r.output.len()
        );
        assert!(
            !r.output.contains("... (content truncated)"),
            "the internal blind cut must not fire on the armed path"
        );
    }

    #[tokio::test]
    async fn should_fire_the_line_limit_first_on_a_many_short_lines_file_when_armed() {
        let dir = tempfile::tempdir().unwrap();
        let many = (1..=3000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("many_armed.txt"), &many).unwrap();
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);

        let r = tool
            .execute(&serde_json::json!({"path": "many_armed.txt"}))
            .await
            .unwrap();

        assert!(r.success, "{}", r.output);
        assert!(
            r.output.contains("line 2000"),
            "line 2000 is the last shown"
        );
        assert!(!r.output.contains("line 2001"), "line 2001 is windowed off");
        assert!(
            r.output.contains("showing lines 1-2000 of 3000"),
            "footer names the range and total: {}",
            r.output
        );
        assert!(
            r.output.contains("2000-line limit"),
            "the footer names WHICH limit fired (lines, not bytes): {}",
            r.output
        );
        assert!(r.output.contains("offset: 2001"), "{}", r.output);
        assert!(r.output.len() <= octos_core::tool_output_limit("read_file"));
    }

    #[tokio::test]
    async fn should_clamp_an_explicit_oversized_limit_when_armed() {
        let dir = tempfile::tempdir().unwrap();
        wide_rows_file(&dir, "clamp_armed.txt");
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);

        let r = tool
            .execute(&serde_json::json!({"path": "clamp_armed.txt", "offset": 1, "limit": 999999}))
            .await
            .unwrap();

        assert!(r.success, "{}", r.output);
        assert!(
            r.output.contains("showing lines 1-450 of 1500") && r.output.contains("offset: 451"),
            "an explicit range past the window is clamped with the same footer: {}",
            r.output
        );
        assert!(r.output.len() <= octos_core::tool_output_limit("read_file"));
    }

    #[tokio::test]
    async fn should_continue_from_a_later_offset_with_the_same_window_when_armed() {
        // The continuation call the footer names must itself work and name
        // the next one — that is what makes paging converge.
        let dir = tempfile::tempdir().unwrap();
        wide_rows_file(&dir, "page2_armed.txt");
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);

        let r = tool
            .execute(&serde_json::json!({"path": "page2_armed.txt", "offset": 451}))
            .await
            .unwrap();

        assert!(r.success, "{}", r.output);
        assert!(
            r.output.contains("row 000451"),
            "page two starts where told"
        );
        assert!(
            r.output.contains("showing lines 451-900 of 1500") && r.output.contains("offset: 901"),
            "page two names page three: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn should_return_small_files_whole_and_byte_identical_when_armed() {
        // Arming must not touch anything that fits the window: same bytes as
        // the unarmed goldens captured before this feature existed.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("golden_small.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let ten = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("golden_range.txt"), &ten).unwrap();
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);

        let small = tool
            .execute(&serde_json::json!({"path": "golden_small.txt"}))
            .await
            .unwrap();
        assert!(small.success);
        assert_eq!(
            (
                small.output.len(),
                FileStateCache::content_hash(small.output.as_bytes())
            ),
            (32, 0xa9a1_582d_5fdd_6b1c),
            "armed read of a small file must be byte-identical to unarmed: {:?}",
            small.output
        );

        let range = tool
            .execute(
                &serde_json::json!({"path": "golden_range.txt", "start_line": 3, "end_line": 5}),
            )
            .await
            .unwrap();
        assert!(range.success);
        assert_eq!(
            (
                range.output.len(),
                FileStateCache::content_hash(range.output.as_bytes())
            ),
            (62, 0x7ca7_68c2_04c1_08d7),
            "armed in-window explicit range must be byte-identical to unarmed: {:?}",
            range.output
        );
    }

    #[tokio::test]
    async fn should_keep_unarmed_outputs_byte_identical_to_pre_change_goldens() {
        // Golden compare against a capture taken on the pre-change tree
        // (fnv-1a via FileStateCache::content_hash, plus exact lengths).
        // Inputs are reconstructed deterministically; outputs embed only the
        // relative path, so the hashes are stable across hosts.
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadFileTool::new(dir.path());

        std::fs::write(dir.path().join("golden_small.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let ten = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("golden_range.txt"), &ten).unwrap();
        std::fs::write(
            dir.path().join("golden_big.txt"),
            "0123456789abcdef\n".repeat(4000),
        )
        .unwrap();
        let wide = (0..3000)
            .map(|_| "x".repeat(40))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("golden_cut.txt"), &wide).unwrap();
        let many = (1..=3000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("golden_manylines.txt"), &many).unwrap();

        // (args, success, output_len, fnv1a) captured pre-change:
        let cases: Vec<(serde_json::Value, bool, usize, u64)> = vec![
            (
                serde_json::json!({"path": "golden_small.txt"}),
                true,
                32,
                0xa9a1_582d_5fdd_6b1c,
            ),
            (
                serde_json::json!({"path": "golden_range.txt", "start_line": 3, "end_line": 5}),
                true,
                62,
                0x7ca7_68c2_04c1_08d7,
            ),
            // The #2131 refusal for an oversized unbounded read stays.
            (
                serde_json::json!({"path": "golden_big.txt"}),
                false,
                305,
                0x1fb1_380c_4950_1cb6,
            ),
            // The internal blind 100KB cut stays on the unarmed path.
            (
                serde_json::json!({"path": "golden_cut.txt", "start_line": 1, "end_line": 3000}),
                true,
                100_024,
                0xfd5e_1b93_81ff_e0b0,
            ),
            // >2000 lines unbounded stays a FULL read when unarmed.
            (
                serde_json::json!({"path": "golden_manylines.txt"}),
                true,
                52_893,
                0x88cc_44e4_d1e6_85a3,
            ),
        ];
        for (args, success, len, fnv) in cases {
            let r = tool.execute(&args).await.unwrap();
            assert_eq!(
                (
                    r.success,
                    r.output.len(),
                    FileStateCache::content_hash(r.output.as_bytes())
                ),
                (success, len, fnv),
                "unarmed output changed for {args}: {:?}...",
                octos_core::truncated_utf8(&r.output, 200, "")
            );
        }
    }

    #[tokio::test]
    async fn should_advise_byte_mode_for_a_single_line_larger_than_the_window_when_armed() {
        // A line bigger than the whole byte window cannot be paged by line
        // offset — the answer is the IN-TOOL raw byte mode, never a shell
        // fallback: shell output is capped at 30,000 bytes
        // (tool_output_limit("shell")), so a `head -c 49152` could never
        // arrive intact even if advised.
        let dir = tempfile::tempdir().unwrap();
        let giant = format!("short first\n{}\nafter line", "G".repeat(60_000));
        std::fs::write(dir.path().join("giant_armed.txt"), &giant).unwrap();
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);

        // Page one: the giant line does not fit after line 1, so the window
        // stops before it and resumes AT it.
        let page1 = tool
            .execute(&serde_json::json!({"path": "giant_armed.txt"}))
            .await
            .unwrap();
        assert!(page1.success, "{}", page1.output);
        assert!(page1.output.contains("short first"));
        assert!(
            !page1.output.contains("GGGG"),
            "the giant line must not leak into page one"
        );
        assert!(
            page1.output.contains("showing lines 1-1 of 3") && page1.output.contains("offset: 2"),
            "page one stops before the giant line and names it as the next offset: {}",
            page1.output
        );

        // Page two starts AT the giant line: advice naming the byte-mode
        // continuation, not content.
        let page2 = tool
            .execute(&serde_json::json!({"path": "giant_armed.txt", "offset": 2}))
            .await
            .unwrap();
        assert!(page2.success, "{}", page2.output);
        assert!(
            !page2.output.contains("GGGG"),
            "a line larger than the window is never returned inline by line mode"
        );
        assert!(
            page2.output.contains("line 2 is 60000 bytes"),
            "the advice names the line and its full size: {}",
            page2.output
        );
        assert!(
            page2.output.contains("byte_offset: 12"),
            "the advice names the exact byte offset where the line starts: {}",
            page2.output
        );
        assert!(
            !page2.output.contains("sed"),
            "no shell fallback — it cannot survive the shell tool's own \
             30,000-byte cap: {}",
            page2.output
        );
        assert!(
            page2.output.contains("offset: 3"),
            "the advice names how to continue past the giant line: {}",
            page2.output
        );
        assert!(page2.output.len() <= octos_core::tool_output_limit("read_file"));

        // And the advised byte-mode call actually returns the line's bytes.
        let bytes = tool
            .execute(
                &serde_json::json!({"path": "giant_armed.txt", "byte_offset": 12, "byte_limit": 20}),
            )
            .await
            .unwrap();
        assert!(bytes.success, "{}", bytes.output);
        assert!(
            bytes.output.starts_with(&"G".repeat(20)),
            "raw byte mode returns the giant line's bytes without a gutter: {}",
            octos_core::truncated_utf8(&bytes.output, 120, "...")
        );
        assert!(
            bytes.output.contains("byte_offset: 32"),
            "the byte-mode footer names the exact next byte: {}",
            bytes.output
        );
    }

    #[tokio::test]
    async fn should_page_raw_bytes_with_byte_offset() {
        // The raw byte mode itself: exact slices, an exact continuation,
        // no footer at EOF, UTF-8 boundary snapping, and clean errors for
        // out-of-range or ambiguous parameters. R6: byte mode is part of the
        // ARMED feature (unarmed the schema does not advertise it), so the
        // tool is armed here.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bytes.txt"), "abcdefghij").unwrap();
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);

        let first = tool
            .execute(&serde_json::json!({"path": "bytes.txt", "byte_offset": 0, "byte_limit": 4}))
            .await
            .unwrap();
        assert!(first.success, "{}", first.output);
        assert!(
            first.output.starts_with("abcd") && !first.output.starts_with("abcde"),
            "exactly the requested slice: {}",
            first.output
        );
        assert!(
            first.output.contains("bytes 0-3 of 10") && first.output.contains("byte_offset: 4"),
            "the footer names the actual range, the total, and the next \
             call: {}",
            first.output
        );

        let rest = tool
            .execute(&serde_json::json!({"path": "bytes.txt", "byte_offset": 4}))
            .await
            .unwrap();
        assert!(rest.success, "{}", rest.output);
        assert_eq!(
            rest.output, "efghij",
            "reading to EOF returns the remainder with NO footer — footer \
             absence is the completion signal"
        );

        // UTF-8: an offset inside a multi-byte char snaps BACK to the char
        // boundary (re-serving at most 3 bytes; never leaving a gap).
        std::fs::write(dir.path().join("utf8.txt"), "αβγ").unwrap();
        let snapped = tool
            .execute(&serde_json::json!({"path": "utf8.txt", "byte_offset": 3}))
            .await
            .unwrap();
        assert!(snapped.success, "{}", snapped.output);
        assert_eq!(
            snapped.output, "βγ",
            "offset 3 is inside β (bytes 2..4) — snap back to 2, never split \
             a character"
        );

        // Out of range and ambiguous parameter combinations are clean errors.
        let beyond = tool
            .execute(&serde_json::json!({"path": "bytes.txt", "byte_offset": 100}))
            .await
            .unwrap();
        assert!(!beyond.success);
        assert!(
            beyond.output.contains("beyond"),
            "past-EOF byte_offset is a clean, explained error: {}",
            beyond.output
        );

        let mixed = tool
            .execute(&serde_json::json!({"path": "bytes.txt", "byte_offset": 0, "start_line": 1}))
            .await
            .unwrap();
        assert!(!mixed.success);
        assert!(
            mixed.output.contains("not both"),
            "line and byte parameters are mutually exclusive: {}",
            mixed.output
        );

        let orphan_limit = tool
            .execute(&serde_json::json!({"path": "bytes.txt", "byte_limit": 4}))
            .await
            .unwrap();
        assert!(
            !orphan_limit.success,
            "byte_limit without byte_offset must be rejected: {}",
            orphan_limit.output
        );
    }

    #[tokio::test]
    async fn should_not_serve_file_unchanged_for_a_byte_mode_read() {
        // The M8.4 cache stores LINE ranges; a byte-mode request must bypass
        // it entirely — a cached complete entry must not answer a byte
        // request with the [FILE_UNCHANGED] stub, and a byte read must not
        // poison the line-range cache.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cached.txt"), "one\ntwo\nthree\n").unwrap();
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let cache = Arc::new(FileStateCache::new());
        let ctx = ctx_with_cache(cache.clone());

        let full = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "cached.txt"}))
            .await
            .unwrap();
        assert!(full.success && !full.output.contains("[FILE_UNCHANGED]"));

        let bytes = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "cached.txt", "byte_offset": 0, "byte_limit": 3}),
            )
            .await
            .unwrap();
        assert!(bytes.success, "{}", bytes.output);
        assert!(
            !bytes.output.contains("[FILE_UNCHANGED]"),
            "a byte-mode read must never be answered from the line-range \
             cache: {}",
            bytes.output
        );
        assert!(bytes.output.starts_with("one"), "{}", bytes.output);
    }

    #[tokio::test]
    async fn should_keep_every_armed_return_under_the_loop_cap_for_a_pathological_path() {
        // Path SPELLINGS are caller-controlled and unbounded — a spelling
        // made of thousands of `./` components resolves to a normal file but
        // would blow the output budget if any armed return interpolated it
        // raw. Every armed return must stay under the loop cap regardless.
        let dir = tempfile::tempdir().unwrap();
        let giant = format!("{}\nafter", "G".repeat(60_000));
        std::fs::write(dir.path().join("g.txt"), &giant).unwrap();
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let pathological = format!("{}g.txt", "./".repeat(25_000)); // 50,005 chars

        let advice = tool
            .execute(&serde_json::json!({"path": pathological}))
            .await
            .unwrap();
        assert!(advice.success, "{}", advice.output);
        assert!(
            advice.output.contains("byte_offset: 0"),
            "the giant-first-line advice still names the byte continuation: {}",
            advice.output
        );
        assert!(
            advice.output.len() <= octos_core::tool_output_limit("read_file"),
            "an armed return may never exceed the loop cap, whatever the \
             path spelling: {} bytes",
            advice.output.len()
        );
    }

    #[tokio::test]
    async fn should_not_cache_a_windowed_read_as_complete_when_armed() {
        // (b) The file-state cache hazard: a windowed read recorded as "no
        // range = complete file" would make the next unbounded read return
        // `[FILE_UNCHANGED] (full file cached)` — a lie about a view the
        // model never fully saw. The recorded view must be the RETURNED
        // window, so an unbounded re-read re-pages instead of claiming
        // completeness.
        let dir = tempfile::tempdir().unwrap();
        wide_rows_file(&dir, "cache_armed.txt");
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let cache = Arc::new(FileStateCache::new());
        let ctx = ctx_with_cache(cache.clone());

        let first = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "cache_armed.txt"}))
            .await
            .unwrap();
        assert!(first.success, "{}", first.output);
        assert!(first.output.contains("showing lines 1-450 of 1500"));

        let second = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "cache_armed.txt"}))
            .await
            .unwrap();
        assert!(second.success, "{}", second.output);
        assert!(
            !second.output.contains("[FILE_UNCHANGED]"),
            "a windowed view must never satisfy an unbounded request as \
             unchanged-complete: {}",
            second.output
        );
        assert!(
            second.output.contains("showing lines 1-450 of 1500"),
            "the honest answer is the same first page again: {}",
            second.output
        );
    }

    #[tokio::test]
    async fn should_still_serve_file_unchanged_for_a_repeated_in_window_range_when_armed() {
        // Arming must not destroy the M8.4 cache win for ranges the model
        // truly saw in full.
        let dir = tempfile::tempdir().unwrap();
        ten_lines_file(&dir);
        let tool = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let cache = Arc::new(FileStateCache::new());
        let ctx = ctx_with_cache(cache.clone());

        let first = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "lines.txt", "start_line": 3, "end_line": 5}),
            )
            .await
            .unwrap();
        assert!(first.success && !first.output.contains("[FILE_UNCHANGED]"));

        let second = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "lines.txt", "start_line": 3, "end_line": 5}),
            )
            .await
            .unwrap();
        assert!(
            second.output.contains("[FILE_UNCHANGED]"),
            "an identical fully-seen range still hits the cache when armed: {}",
            second.output
        );
    }
}
