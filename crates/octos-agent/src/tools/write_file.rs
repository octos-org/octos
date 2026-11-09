//! Write file tool for creating new files.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tracing::warn;

use super::write_grant::WritePathGrant;
use super::{ConcurrencyClass, Tool, ToolContext, ToolResult};
use crate::policy::{FileAccessMode, FilesystemScope};

/// Tool for writing/creating files.
pub struct WriteFileTool {
    /// Base directory for resolving relative paths.
    base_dir: PathBuf,
    /// Effective filesystem scope.
    filesystem_scope: FilesystemScope,
    /// Whether writes are permitted.
    file_access: FileAccessMode,
    /// #1976 — optional per-path write fence. `None` (default, every
    /// pre-#1976 construction) = writes governed by scope/access alone.
    write_grant: Option<WritePathGrant>,
    /// Partial-view overwrite guard (#1638), armed with windowed reads.
    /// `None` = the `OCTOS_READ_WINDOW` env flag decides (production);
    /// `Some` = explicit, for tests.
    window_enforcement: Option<bool>,
}

impl WriteFileTool {
    /// Create a new write file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadWrite,
            write_grant: None,
            window_enforcement: None,
        }
    }

    /// Test-only arming override for the partial-view overwrite guard — see
    /// `ReadFileTool::with_window_enforcement` for why this is per-instance
    /// rather than process-global.
    #[cfg(test)]
    pub(crate) fn with_window_enforcement(mut self, armed: bool) -> Self {
        self.window_enforcement = Some(armed);
        self
    }

    /// Whether the partial-view overwrite guard is armed for this instance.
    fn window_armed(&self) -> bool {
        self.window_enforcement
            .unwrap_or_else(super::read_window::armed_from_env)
    }

    /// Set the effective filesystem scope.
    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    /// Set the effective file access mode.
    pub fn with_file_access(mut self, file_access: FileAccessMode) -> Self {
        self.file_access = file_access;
        self
    }

    /// #1976 — bind a per-path write fence: only allowlisted
    /// workspace-relative paths are writable (canonical match, deny-wins on
    /// top of scope/access); under `create_only` they may be CREATED but
    /// never overwritten (`O_CREAT|O_EXCL`).
    pub fn with_write_grant(mut self, write_grant: WritePathGrant) -> Self {
        self.write_grant = Some(write_grant);
        self
    }
}

#[derive(Debug, Deserialize)]
// #1770: unknown keys are usually a typo of a real parameter; rejecting
// them (with a did-you-mean via `args::parse_tool_args`) lets the model
// self-correct instead of silently dropping its intent.
#[serde(deny_unknown_fields)]
struct WriteFileInput {
    /// #1767: `filePath` is the industry-convention alias.
    #[serde(alias = "filePath")]
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    // #1638 R6: conditional so the UNARMED spec is byte-identical to
    // origin/main (the ToolSpec is serialized into the prompt-cache prefix for
    // every session). R5: the ARMED description is tool-agnostic — it must NOT
    // name edit_file/apply_patch, which some armed contexts (the slides
    // prompt) forbid; naming a forbidden tool would be unfollowable advice.
    fn description(&self) -> &str {
        if self.window_armed() {
            "Write content to a file. Creates the file if it doesn't exist, or overwrites if it \
             does. Overwrites the ENTIRE file: never write back a file you have only partially \
             read — finish reading the whole file first, then retry."
        } else {
            "Write content to a file. Creates the file if it doesn't exist, or overwrites if it \
             does."
        }
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        // Writing to disk mutates state visible to every other tool. If a
        // parallel `read_file` targets the same path we'd hand the LLM a
        // torn view. Serialize the whole batch. See M8.8.
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (alias: filePath)"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["path", "content"]
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
        let mut result = self.execute_capped_inner(ctx, args).await?;
        // #2193 R4: ONE release-enforced cap over EVERY armed return (success
        // and early errors). A caller-controlled `input.path` echoed in a
        // message can otherwise exceed the loop's blind head/tail cut (#2124)
        // and get mangled; sizing every armed result under the cap makes the
        // tool's own cut the only cut.
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

impl WriteFileTool {
    async fn execute_capped_inner(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let input: WriteFileInput =
            super::args::parse_tool_args(self.name(), &self.input_schema(), args)?;

        if !self.file_access.allows_write() {
            return Ok(ToolResult {
                output: "write_file is not permitted by read-only filesystem access".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // Phase 2-C of the SessionScope migration: when the host has
        // threaded a scope through `ToolContext`, use it as the single
        // source of truth for base_dir + path classification. WRITES
        // are permitted only for `InWorkspace` and `InGrantedDir`;
        // `InSharedZone` and `OutOfScope` are refused. The shared
        // helper canonicalizes the candidate before classification so
        // ancestor symlinks can't smuggle a write out of the workspace
        // (`O_NOFOLLOW` only protects the final component). This also
        // fixes the path asymmetry that #1189 worked around:
        // write_file now writes under `scope.workspace()` — the same
        // directory plugin tools run in.
        let path = match ctx.session_scope.as_ref() {
            Some(scope) => match super::resolve_path_for_session_scope_write(scope, &input.path) {
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

        // Never let a file tool touch a `.git` directory: its config/hooks
        // decide what the kernel's own git invocations execute.
        if let Err(reason) = super::refuse_git_internal_path(&path) {
            return Ok(ToolResult {
                output: reason,
                success: false,
                ..Default::default()
            });
        }

        // Observe-only (#read-paging probe): a whole-file overwrite of a path
        // that was previously read. If `read_file` were ever changed to return
        // a WINDOW by default, this is the call that would reconstruct the file
        // from an incomplete view and destroy the tail the model never saw —
        // the slides workflow does exactly read-then-rebuild-then-write. The
        // probe records it; nothing here changes.
        if super::read_paging_probe::enabled() && path.exists() {
            let tripped = super::read_paging_probe::record_overwrite(&path.to_string_lossy());
            if tripped {
                tracing::warn!(
                    path = %path.display(),
                    "read-paging probe: overwriting a file whose earlier read would have been \
                     PARTIAL under a forced window"
                );
            }
        }

        // #1638 (c): armed FAIL-CLOSED overwrite guard. write_file
        // reconstructs a file from whatever the model saw, so overwriting an
        // existing over-window file requires a COMPLETE, current-epoch,
        // untainted view from THIS session — no ledger entry means refuse
        // and read first (which makes restart and eviction safe by
        // construction: absence can only cost a re-read, never an unseen
        // overwrite). Files at or under the byte window can still be
        // blind-overwritten exactly as today, but any recorded view of them
        // is honoured. The advice names only read_file — the calling context
        // may forbid every other tool (the slides prompt allows nothing but
        // read_file/write_file for authoring). Sits BEFORE the write fence
        // and any disk mutation; the #2126 probe block above stays
        // observe-only and untouched (an armed refusal still increments its
        // attempt counter — the probe measures intent, not damage).
        //
        // A COMPLETE authorization also yields the epoch it was granted
        // against, which the write below binds to via `write_no_follow_checked`
        // (R1: closes the authorize→truncate TOCTOU).
        let mut authorized_epoch: Option<super::read_window::ViewEpoch> = None;
        if self.window_armed() && path.exists() {
            use super::read_window::{
                PARTIAL_VIEW_OVERWRITE_PREFIX as PREFIX,
                REDACTED_VIEW_OVERWRITE_PREFIX as REDACTED_PREFIX,
                TRANSFORMED_VIEW_OVERWRITE_PREFIX as TRANSFORMED_PREFIX, ViewStatus,
                WINDOW_MAX_BYTES,
            };
            // R4: a missing/empty session key is reachable in production
            // (agents default to None; FFI/fleet workers; plain CLI chat
            // without --goals). Keyless tasks share no bucket and record
            // nothing, so they can never establish COMPLETE — fail closed for
            // over-window files rather than let one keyless task authorize
            // another's overwrite.
            let session = ctx.parent_session_key.as_deref().unwrap_or("");
            // The path shown to the model is clamped: path SPELLINGS are
            // caller-controlled and can be arbitrarily long, and a refusal
            // that overflows the loop's output cap gets blind-truncated into
            // useless advice.
            let shown_path = octos_core::truncated_utf8(&input.path, 200, "...");
            let page_advice = "Page through it with read_file (offset/limit, or byte_offset \
                               for single lines too long for the line window) until you reach \
                               the end, then retry the write.";
            let current = tokio::fs::metadata(&path)
                .await
                .ok()
                .and_then(|meta| super::read_window::ViewEpoch::from_metadata(&meta));
            let refusal: Option<String> = if session.is_empty() {
                // Keyless: cannot track reads, so cannot authorize a big
                // overwrite. Small files keep today's blind semantics.
                match current {
                    Some(now) if (now.size as usize) <= WINDOW_MAX_BYTES => None,
                    Some(now) => Some(format!(
                        "{PREFIX} write_file refused: {shown_path} is {} bytes — larger than \
                         the {WINDOW_MAX_BYTES}-byte read window — and this session has no \
                         identity to track what you have read, so a whole-file overwrite \
                         cannot be authorized. Start the session with a goal/session key, or \
                         make a narrower change.",
                        now.size,
                    )),
                    None => Some(format!(
                        "{PREFIX} write_file refused: {shown_path} could not be validated on \
                         disk and this session has no identity to track what you have read. \
                         Re-read it, or make a narrower change.",
                    )),
                }
            } else {
                match (super::read_window::view_status(session, &path), current) {
                    // The one allowed overwrite: complete, untainted, and the
                    // file on disk is still the generation the model read.
                    // Bind the write to this epoch (R1).
                    (ViewStatus::Complete { epoch }, Some(now)) if epoch == now => {
                        authorized_epoch = Some(now);
                        None
                    }
                    (ViewStatus::Complete { .. }, _) => Some(format!(
                        "{PREFIX} write_file refused: {shown_path} has changed on disk since \
                         you last read it (or its current state could not be validated). \
                         Re-read it with read_file, then retry the write.",
                    )),
                    // R2: TAINTED is a DISTINCT failure — the loop sanitizer
                    // redacts every raw byte page too, so paging can NEVER
                    // deliver these bytes whole. Its own prefix, and advice
                    // that does NOT say "page through" (which would loop
                    // forever): the flag is incompatible with rewriting this
                    // file.
                    (ViewStatus::Tainted, _) => Some(format!(
                        "{REDACTED_PREFIX} write_file refused: {shown_path} contains content \
                         the harness redacts from tool output (embedded data or credentials), \
                         so read_file cannot deliver it to you whole — no amount of paging \
                         will. The windowed-read flag is incompatible with reconstructing and \
                         rewriting THIS file: make a narrower, in-place change rather than a \
                         whole-file rewrite, or tell the user this file cannot be safely \
                         whole-file-rewritten while windowed reads are enabled.",
                    )),
                    // #2193 R4: the view was TEXT decoded from a binary (PDF),
                    // not the on-disk bytes — paging the extracted text to EOF
                    // must NOT authorize replacing the original binary.
                    (ViewStatus::Transformed, _) => Some(format!(
                        "{TRANSFORMED_PREFIX} write_file refused: read_file showed you TEXT \
                         extracted from {shown_path} (a PDF or other binary), not the file's \
                         own bytes — so a whole-file overwrite reconstructed from what you saw \
                         would replace the original with the extracted text and destroy the \
                         binary. Make a narrower change, write to a different path, or tell the \
                         user this file cannot be whole-file-rewritten from a windowed read.",
                    )),
                    (
                        ViewStatus::Partial {
                            seen_through,
                            total_bytes,
                        },
                        _,
                    ) => Some(format!(
                        "{PREFIX} write_file refused: read_file has shown you only part of \
                         {shown_path} (the first {seen_through} of its {total_bytes} bytes), \
                         and overwriting the whole file from a partial view would destroy the \
                         part you have not seen. {page_advice}",
                    )),
                    (ViewStatus::Unknown, Some(now)) if now.size as usize > WINDOW_MAX_BYTES => {
                        Some(format!(
                            "{PREFIX} write_file refused: {shown_path} is {} bytes — larger \
                             than the {WINDOW_MAX_BYTES}-byte read window — and you have not \
                             read it in this session. {page_advice}",
                            now.size,
                        ))
                    }
                    // Existing file whose metadata cannot be read: size cannot
                    // be validated, so fail closed rather than guess.
                    (ViewStatus::Unknown, None) => Some(format!(
                        "{PREFIX} write_file refused: {shown_path} has changed on disk since \
                         you last read it (or its current state could not be validated). \
                         Re-read it with read_file, then retry the write.",
                    )),
                    // Never-read file at or under the window: blind overwrite
                    // is today's documented write_file semantics.
                    (ViewStatus::Unknown, Some(_)) => None,
                }
            };
            if let Some(output) = refusal {
                // R5: real runtime clamp on the armed refusal (release builds
                // drop debug_assert). shown_path is already bounded to ~200
                // bytes, so this never actually cuts; it is the enforced
                // backstop keeping the loop's blind cut off the advice.
                let output = octos_core::truncated_utf8(
                    &output,
                    octos_core::tool_output_limit("write_file"),
                    "",
                );
                return Ok(ToolResult {
                    output,
                    success: false,
                    ..Default::default()
                });
            }
        }

        let created = !path.exists();
        // A fenced write must never reopen its lexical path for preview
        // bytes: O_NOFOLLOW on the leaf cannot stop an ancestor swap. Keep
        // its confined-write authority intact and omit overwrite baselines.
        let previous_content = if created || self.write_grant.is_some() {
            None
        } else {
            super::read_file_mutation_previous_text(&path).await
        };

        // #1976 — per-path write fence. SECURITY ROUND (codex): the allowlist
        // decision and the actual open must target the SAME resolved object,
        // or an attacker who swaps a checked ancestor dir for a symlink
        // between check and open escapes (leaf `O_NOFOLLOW` guards only the
        // leaf). So a fenced write does NOT reuse the generic lexical
        // `write_no_follow` below: `check_write` returns the workspace-relative
        // path, and `confined_write` re-opens it via a component-wise
        // `O_NOFOLLOW` `openat` walk — a symlinked (or swapped-to-symlink)
        // ancestor fails `ELOOP`/`ENOTDIR` at its own component. `create_only`
        // becomes `O_CREAT|O_EXCL` on that same walked leaf.
        let fenced = if let Some(grant) = &self.write_grant {
            let workspace_root = ctx
                .session_scope
                .as_ref()
                .map(|scope| scope.workspace().to_path_buf())
                .unwrap_or_else(|| self.base_dir.clone());
            let rel = match grant.check_write(&workspace_root, &path, &input.path, self.name()) {
                Ok(rel) => rel,
                Err(denied) => {
                    return Ok(ToolResult {
                        output: denied,
                        success: false,
                        ..Default::default()
                    });
                }
            };
            // #2193 R4: an armed, AUTHORIZED over-window overwrite must bind the
            // truncating write to the epoch it was authorized against — even on
            // the fenced path, which otherwise re-opens the leaf with O_TRUNC and
            // clobbers whatever it resolves to. Only existing-file overwrites
            // (never create_only) take the checked path.
            match (grant.create_only(), authorized_epoch) {
                (false, Some(expected)) => {
                    match super::write_grant::confined_write_checked(
                        workspace_root.clone(),
                        rel,
                        input.content.as_bytes().to_vec(),
                        expected,
                    )
                    .await
                    {
                        Ok(super::CheckedWrite::Written) => {}
                        Ok(super::CheckedWrite::EpochChanged { found }) => {
                            if let Some(session) =
                                ctx.parent_session_key.as_deref().filter(|s| !s.is_empty())
                            {
                                super::read_window::forget(session, &path);
                            }
                            let shown_path = octos_core::truncated_utf8(&input.path, 200, "...");
                            return Ok(ToolResult {
                                output: format!(
                                    "{} write_file refused: {} changed on disk between your read \
                                     and this write ({} bytes now), so the overwrite was not \
                                     applied. Re-read it with read_file, then retry.",
                                    super::read_window::PARTIAL_VIEW_OVERWRITE_PREFIX,
                                    shown_path,
                                    found.size,
                                ),
                                success: false,
                                ..Default::default()
                            });
                        }
                        Err(e) => {
                            return Ok(ToolResult {
                                output: grant.map_confined_error(
                                    &e,
                                    &workspace_root,
                                    &input.path,
                                    self.name(),
                                ),
                                success: false,
                                ..Default::default()
                            });
                        }
                    }
                }
                _ => {
                    if let Err(e) = super::write_grant::confined_write(
                        workspace_root.clone(),
                        rel,
                        input.content.as_bytes().to_vec(),
                        grant.create_only(),
                    )
                    .await
                    {
                        return Ok(ToolResult {
                            output: grant.map_confined_error(
                                &e,
                                &workspace_root,
                                &input.path,
                                self.name(),
                            ),
                            success: false,
                            ..Default::default()
                        });
                    }
                }
            }
            true
        } else {
            false
        };

        if !fenced {
            // Create parent directories if needed (generic, unfenced path).
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.wrap_err_with(|| {
                    // Bound the caller-derived path so an armed Err stays under
                    // the tool-output cap (#2193 R4).
                    format!(
                        "failed to create directories: {}",
                        octos_core::truncated_utf8(&parent.display().to_string(), 200, "…")
                    )
                })?;
            }
            // #1638 R1: when the armed guard authorized this overwrite against
            // a specific epoch, bind the truncating write to the exact opened
            // inode — a replacement in the authorize→truncate window is
            // refused, not clobbered. Non-authorized writes (new files, small
            // never-read files under today's blind semantics) take the plain
            // O_NOFOLLOW path.
            match authorized_epoch {
                Some(expected) => {
                    match super::write_no_follow_checked(&path, input.content.as_bytes(), expected)
                        .await
                    {
                        Ok(super::CheckedWrite::Written) => {}
                        Ok(super::CheckedWrite::EpochChanged { found }) => {
                            // The inode we were authorized against is gone;
                            // forget the now-invalid coverage and refuse.
                            if let Some(session) =
                                ctx.parent_session_key.as_deref().filter(|s| !s.is_empty())
                            {
                                super::read_window::forget(session, &path);
                            }
                            let shown_path = octos_core::truncated_utf8(&input.path, 200, "...");
                            return Ok(ToolResult {
                                output: format!(
                                    "{} write_file refused: {} changed on disk between your read \
                                     and this write ({} bytes now), so the overwrite was not \
                                     applied. Re-read it with read_file, then retry.",
                                    super::read_window::PARTIAL_VIEW_OVERWRITE_PREFIX,
                                    shown_path,
                                    found.size,
                                ),
                                success: false,
                                ..Default::default()
                            });
                        }
                        Err(e) => return Ok(super::file_io_error(e, &input.path)),
                    }
                }
                None => {
                    // Write file (O_NOFOLLOW atomically rejects symlinks, no TOCTOU race).
                    if let Err(e) = super::write_no_follow(&path, input.content.as_bytes()).await {
                        return Ok(super::file_io_error(e, &input.path));
                    }
                }
            }
        }

        // #1976 SECURITY ROUND 2 (codex): a per-path write fence makes this a
        // LEAF-FILE operation, not a project mutation — so the post-write
        // processors that re-resolve the LEXICAL path are SKIPPED under a
        // fence. Both would breach the fence: the formatter would run an
        // external tool on a lexical filename, and `snapshot_workspace_change`
        // lexically derives a repo root and unconditionally creates
        // dirs/`.gitignore`/`.git`/objects/commits at NON-granted sibling
        // paths (and an ancestor swap before it re-opens the very TOCTOU the
        // confined write just closed). A fenced worker is allowlisted to
        // specific files; it must not trigger repo snapshotting of
        // lexically-derived siblings. Cache invalidation stays — it is a pure
        // in-memory, path-keyed operation with no filesystem re-resolution.
        // `fenced` (set on the confined-write path above) is true iff a fence
        // is bound: reaching here with `write_grant` Some implies `fenced`.

        // #1774: opt-in post-edit formatting. Runs BEFORE cache invalidation
        // and the git snapshot so both observe the final on-disk content.
        // Best-effort by contract — a formatter failure never fails the write.
        // Never runs under a fence (see above).
        let format_note = if ctx.format_after_edit && !fenced {
            crate::format::post_edit_format_note(&path, &input.content).await
        } else {
            None
        };

        // M8.4: invalidate any stale cache entry for this path — the file's
        // contents (and mtime) just changed, so previous reads must not serve
        // a [FILE_UNCHANGED] stub on the next read.
        if let Some(cache) = ctx.file_state_cache.as_ref() {
            cache.invalidate(&path);
        }

        if !fenced {
            if let Err(error) =
                crate::workspace_git::snapshot_workspace_change(&self.base_dir, &path, "write_file")
            {
                warn!(
                    path = %input.path,
                    error = %error,
                    "workspace git snapshot failed after write_file"
                );
            }
        }

        // #1638 (c): a successful whole-file write makes the on-disk content
        // exactly what the model supplied, so record the view COMPLETE at
        // the post-write epoch — under the fail-closed rule, merely
        // forgetting the path would refuse the model's next overwrite of a
        // big file it authored one call ago. Exception: when the post-edit
        // formatter rewrote the file, the on-disk bytes are no longer what
        // the model supplied (the note echoes at most a truncated preview),
        // so the path is forgotten instead — absence refuses, and the model
        // re-reads the formatted result, which is correct.
        if self.window_armed() {
            let session = ctx.parent_session_key.clone().unwrap_or_default();
            if format_note.is_none() {
                super::read_window::note_full_write(&session, &path, input.content.len());
            } else {
                super::read_window::forget(&session, &path);
            }
        }

        let line_count = input.content.lines().count();
        Ok(ToolResult {
            output: format!(
                "Successfully wrote {} lines to {}{}",
                line_count,
                octos_core::truncated_utf8(&input.path, 200, "..."),
                format_note.unwrap_or_default()
            ),
            success: true,
            file_modified: Some(path),
            structured_metadata: Some(serde_json::json!({
                "file_created": created,
                "previous_content": previous_content,
            })),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // M8.4 integration test — write invalidates the file-state cache.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Phase 2-C: SessionScope integration tests for WriteFileTool.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // #1976: per-path write-grant enforcement.
    // -----------------------------------------------------------------------
    #[cfg(unix)]
    fn fenced_tool(dir: &std::path::Path, patterns: &[&str], create_only: bool) -> WriteFileTool {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        WriteFileTool::new(dir).with_write_grant(
            crate::tools::write_grant::WritePathGrant::new(&owned, create_only)
                .expect("test grant compiles"),
        )
    }

    #[cfg(unix)]
    #[test]
    fn write_grant_ancestor_swap_cannot_capture_external_preview_baseline() {
        use std::os::unix::fs::symlink;
        use std::task::Poll;

        // Hold the sole blocking worker between polls. This deterministically
        // swaps the ancestor AFTER path resolution but BEFORE the baseline
        // read, without hooks in production or timing-dependent retries.
        async fn park_worker() -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
            let (started, ready) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let job = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            });
            ready.await.unwrap();
            (release, job)
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let cards = dir.path().join("cards");
            let saved = dir.path().join("saved");
            std::fs::create_dir(&cards).unwrap();
            std::fs::write(cards.join("allowed.txt"), "inside\n").unwrap();
            std::fs::write(
                outside.path().join("allowed.txt"),
                "OUTSIDE_PREVIEW_SECRET\n",
            )
            .unwrap();
            let tool = fenced_tool(dir.path(), &["cards/allowed.txt"], false)
                .with_window_enforcement(false);
            let args = serde_json::json!({"path": "cards/allowed.txt", "content": "new\n"});
            let mut operation = std::pin::pin!(tool.execute(&args));
            let (release, job) = park_worker().await;
            assert!(matches!(futures::poll!(&mut operation), Poll::Pending));
            std::fs::rename(&cards, &saved).unwrap();
            symlink(outside.path(), &cards).unwrap();
            release.send(()).unwrap();
            job.await.unwrap();
            let (release, job) = park_worker().await;
            let result = match futures::poll!(&mut operation) {
                Poll::Ready(result) => {
                    release.send(()).unwrap();
                    job.await.unwrap();
                    result.unwrap()
                }
                Poll::Pending => {
                    // Old code has finished stat and scheduled the lexical
                    // read. Let it capture the outside file, then restore the
                    // ancestor before the confined write can check it.
                    release.send(()).unwrap();
                    job.await.unwrap();
                    let (release, job) = park_worker().await;
                    std::fs::remove_file(&cards).unwrap();
                    std::fs::rename(&saved, &cards).unwrap();
                    release.send(()).unwrap();
                    job.await.unwrap();
                    operation.await.unwrap()
                }
            };
            assert_eq!(
                std::fs::read_to_string(outside.path().join("allowed.txt")).unwrap(),
                "OUTSIDE_PREVIEW_SECRET\n"
            );
            assert!(
                result
                    .structured_metadata
                    .as_ref()
                    .is_none_or(|metadata| metadata["previous_content"].is_null()),
                "outside bytes reached mutation metadata: {:?}",
                result.structured_metadata
            );
            assert!(!result.output.contains("OUTSIDE_PREVIEW_SECRET"));
        });
    }

    #[tokio::test]
    async fn write_grant_denies_non_allowlisted_path() {
        // Acceptance (#1976): the same worker CANNOT write app.md — typed
        // `[denied]` refusal, file never created, violation never silent.
        let dir = tempfile::tempdir().unwrap();
        let tool = fenced_tool(dir.path(), &["exemplar.card"], true);

        let result = tool
            .execute(&serde_json::json!({"path": "app.md", "content": "nope\n"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .output
                .contains(crate::tools::write_grant::DENIED_MARKER),
            "refusal must carry the [denied] class: {}",
            result.output
        );
        assert!(
            !dir.path().join("app.md").exists(),
            "refused write must not create the file"
        );
    }

    #[tokio::test]
    async fn write_grant_create_only_refuses_overwrite() {
        // Acceptance (#1976): create_only = O_CREAT|O_EXCL semantics — the
        // first create passes, the second write to the SAME allowlisted path
        // is refused and the content is untouched.
        let dir = tempfile::tempdir().unwrap();
        let tool = fenced_tool(dir.path(), &["exemplar.card"], true);

        let first = tool
            .execute(&serde_json::json!({"path": "exemplar.card", "content": "v1\n"}))
            .await
            .unwrap();
        assert!(first.success, "{}", first.output);

        let second = tool
            .execute(&serde_json::json!({"path": "exemplar.card", "content": "v2\n"}))
            .await
            .unwrap();
        assert!(!second.success, "overwrite must be refused");
        assert!(
            second.output.contains("already exists"),
            "typed create-only refusal: {}",
            second.output
        );
        assert!(
            second
                .output
                .contains(crate::tools::write_grant::DENIED_MARKER)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("exemplar.card")).unwrap(),
            "v1\n",
            "refused overwrite must leave the original bytes"
        );
    }
}
