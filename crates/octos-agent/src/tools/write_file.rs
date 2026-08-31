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

    // #1638 (d): mode-neutral guidance — reconstructing a partially-read file
    // is destructive in every mode; the armed guard merely enforces what this
    // sentence advises.
    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, or overwrites if it \
         does. Overwrites the ENTIRE file: never write back a file you have only partially \
         read — finish reading it first, or use edit_file/apply_patch for targeted changes."
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
        if self.window_armed() && path.exists() {
            use super::read_window::{ViewStatus, WINDOW_MAX_BYTES};
            let session = ctx.parent_session_key.clone().unwrap_or_default();
            // The path shown to the model is clamped: path SPELLINGS are
            // caller-controlled and can be arbitrarily long, and a refusal
            // that overflows the loop's output cap gets blind-truncated into
            // useless advice.
            let shown_path = octos_core::truncated_utf8(&input.path, 200, "...");
            let page_advice = "Page through it with read_file (offset/limit, or byte_offset \
                               for single lines too long for the line window) until you reach \
                               the end, then retry the write.";
            let current = tokio::fs::metadata(&path).await.ok().and_then(|meta| {
                meta.modified()
                    .ok()
                    .map(|mtime| super::read_window::ViewEpoch {
                        mtime,
                        size: meta.len(),
                    })
            });
            let refusal = match (super::read_window::view_status(&session, &path), current) {
                // The one allowed overwrite: complete, untainted, and the
                // file on disk is still the generation the model read.
                (ViewStatus::Complete { epoch }, Some(now)) if epoch == now => None,
                (ViewStatus::Complete { .. }, _) => Some(format!(
                    "{} write_file refused: {} has changed on disk since you last read it (or \
                     its current state could not be validated). Re-read it with read_file, \
                     then retry the write.",
                    super::read_window::PARTIAL_VIEW_OVERWRITE_PREFIX,
                    shown_path,
                )),
                (ViewStatus::Tainted, _) => Some(format!(
                    "{} write_file refused: parts of {} were redacted from your view (embedded \
                     data or credentials), so writing the whole file back would replace them \
                     with redaction placeholders. Make a narrower change instead of a \
                     whole-file rewrite.",
                    super::read_window::PARTIAL_VIEW_OVERWRITE_PREFIX,
                    shown_path,
                )),
                (
                    ViewStatus::Partial {
                        seen_through,
                        total_bytes,
                    },
                    _,
                ) => Some(format!(
                    "{} write_file refused: read_file has shown you only part of {} (the first \
                     {seen_through} of its {total_bytes} bytes), and overwriting the whole \
                     file from a partial view would destroy the part you have not seen. {}",
                    super::read_window::PARTIAL_VIEW_OVERWRITE_PREFIX,
                    shown_path,
                    page_advice,
                )),
                (ViewStatus::Unknown, Some(now)) if now.size as usize > WINDOW_MAX_BYTES => {
                    Some(format!(
                        "{} write_file refused: {} is {} bytes — larger than the \
                         {WINDOW_MAX_BYTES}-byte read window — and you have not read it in \
                         this session. {}",
                        super::read_window::PARTIAL_VIEW_OVERWRITE_PREFIX,
                        shown_path,
                        now.size,
                        page_advice,
                    ))
                }
                // Existing file whose metadata cannot be read: size cannot
                // be validated, so fail closed rather than guess.
                (ViewStatus::Unknown, None) => Some(format!(
                    "{} write_file refused: {} has changed on disk since you last read it (or \
                     its current state could not be validated). Re-read it with read_file, \
                     then retry the write.",
                    super::read_window::PARTIAL_VIEW_OVERWRITE_PREFIX,
                    shown_path,
                )),
                // Never-read file at or under the window: blind overwrite is
                // today's documented write_file semantics.
                (ViewStatus::Unknown, Some(_)) => None,
            };
            if let Some(output) = refusal {
                return Ok(ToolResult {
                    output,
                    success: false,
                    ..Default::default()
                });
            }
        }

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
            if let Err(e) = super::write_grant::confined_write(
                workspace_root.clone(),
                rel,
                input.content.as_bytes().to_vec(),
                grant.create_only(),
            )
            .await
            {
                return Ok(ToolResult {
                    output: grant.map_confined_error(&e, &workspace_root, &input.path, self.name()),
                    success: false,
                    ..Default::default()
                });
            }
            true
        } else {
            false
        };

        if !fenced {
            // Create parent directories if needed (generic, unfenced path).
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.wrap_err_with(|| {
                    format!("failed to create directories: {}", parent.display())
                })?;
            }
            // Write file (O_NOFOLLOW atomically rejects symlinks, no TOCTOU race).
            if let Err(e) = super::write_no_follow(&path, input.content.as_bytes()).await {
                return Ok(super::file_io_error(e, &input.path));
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
                input.path,
                format_note.unwrap_or_default()
            ),
            success: true,
            file_modified: Some(path),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_file_tool_is_exclusive() {
        // write_file mutates disk visible to other tools in the batch,
        // so it must serialize (M8.8).
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new(dir.path());
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Exclusive);
    }

    #[tokio::test]
    async fn test_write_file_creates_new() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"path": "new.txt", "content": "hello world\n"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("Successfully wrote"));
        let content = std::fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, "hello world\n");
    }

    #[tokio::test]
    async fn should_accept_file_path_alias_for_path() {
        // #1767: `filePath` is the industry-convention alias for `path`.
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"filePath": "aliased.txt", "content": "via alias\n"}))
            .await
            .unwrap();

        assert!(
            result.success,
            "filePath alias must work: {}",
            result.output
        );
        let content = std::fs::read_to_string(dir.path().join("aliased.txt")).unwrap();
        assert_eq!(content, "via alias\n");
    }

    #[test]
    fn schema_advertises_canonical_names_only() {
        let tool = WriteFileTool::new("/tmp");
        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("path"));
        assert!(props.contains_key("content"));
        assert!(!props.contains_key("filePath"));
    }

    #[tokio::test]
    async fn test_write_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"path": "a/b/c/deep.txt", "content": "nested\n"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(dir.path().join("a/b/c/deep.txt").exists());
    }

    #[tokio::test]
    async fn test_write_file_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exist.txt"), "old content").unwrap();

        let tool = WriteFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "exist.txt", "content": "new content"}))
            .await
            .unwrap();

        assert!(result.success);
        let content = std::fs::read_to_string(dir.path().join("exist.txt")).unwrap();
        assert_eq!(content, "new content");
    }

    #[tokio::test]
    async fn test_write_file_traversal_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"path": "../escape.txt", "content": "bad"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("outside working directory"));
    }

    #[tokio::test]
    async fn test_write_file_reports_line_count() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"path": "multi.txt", "content": "a\nb\nc\n"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("3 lines"));
    }

    #[test]
    fn test_tool_metadata() {
        let tool = WriteFileTool::new("/tmp");
        assert_eq!(tool.name(), "write_file");
        assert!(tool.tags().contains(&"fs"));
    }

    // -----------------------------------------------------------------------
    // M8.4 integration test — write invalidates the file-state cache.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Phase 2-C: SessionScope integration tests for WriteFileTool.
    // -----------------------------------------------------------------------

    use octos_core::SessionScope;
    use std::sync::Arc;

    fn ctx_with_scope(scope: SessionScope) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "write-with-scope".to_string();
        ctx.session_scope = Some(Arc::new(scope));
        ctx
    }

    #[tokio::test]
    async fn write_file_uses_scope_workspace_as_base_dir_for_relative_paths() {
        // Closes the #1189 asymmetry: when a scope is present, relative
        // writes land under `scope.workspace()`, not the legacy
        // `base_dir`. That's the same directory plugin tools run in,
        // so the rescue heuristic is no longer needed.
        let scope_dir = tempfile::tempdir().unwrap();
        let legacy_dir = tempfile::tempdir().unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = WriteFileTool::new(legacy_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "out.txt", "content": "hi\n"}),
            )
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        // File landed in scope.workspace(), NOT the legacy base_dir.
        assert!(scope_dir.path().join("out.txt").exists());
        assert!(!legacy_dir.path().join("out.txt").exists());
    }

    #[tokio::test]
    async fn write_file_refuses_out_of_scope_path() {
        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_target = outside_dir.path().join("escape.txt");

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = WriteFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": outside_target.to_string_lossy(),
                    "content": "bad\n",
                }),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("outside session scope"),
            "expected scope rejection, got: {}",
            result.output
        );
        assert!(
            !outside_target.exists(),
            "refused write must NOT have created the file"
        );
    }

    #[tokio::test]
    async fn write_file_allows_in_workspace_path() {
        let scope_dir = tempfile::tempdir().unwrap();
        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = WriteFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "ok.txt", "content": "ok\n"}),
            )
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        let body = std::fs::read_to_string(scope_dir.path().join("ok.txt")).unwrap();
        assert_eq!(body, "ok\n");
    }

    #[tokio::test]
    async fn write_file_refuses_write_to_shared_zone() {
        // Multi-tenant shared zones (research/, skills/) are managed
        // by maintenance paths, not session workers. write_file MUST
        // refuse — the symmetry hole that lets a session pollute
        // another tenant's shared data.
        let data_dir = tempfile::tempdir().unwrap();
        let data = data_dir.path().to_path_buf();
        std::fs::create_dir_all(data.join("research")).unwrap();
        std::fs::create_dir_all(data.join("users/web-1/workspace")).unwrap();
        let shared_target = data.join("research/poisoned.md");

        let scope = SessionScope::multi_tenant_with_default_zones(
            data.clone(),
            "dspfac".into(),
            "web-1".into(),
        )
        .unwrap();
        let tool = WriteFileTool::new(scope.workspace());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": shared_target.to_string_lossy(),
                    "content": "bad\n",
                }),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("shared zone"),
            "expected shared-zone rejection, got: {}",
            result.output
        );
        assert!(
            !shared_target.exists(),
            "refused write must NOT have created the file"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_refuses_ancestor_symlink_escape() {
        // Per codex review of the Phase 2-C commit: without
        // ancestor-symlink rejection, a write to `<workspace>/link/x`
        // (where `link` is a symlink pointing outside the workspace)
        // would land at the symlink target — `O_NOFOLLOW` only protects
        // the final component. The shared canonicalizing classifier
        // closes that hole.
        use std::os::unix::fs::symlink;

        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let link_path = scope_dir.path().join("link");
        symlink(outside_dir.path(), &link_path).unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = WriteFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "link/leaked.txt",
                    "content": "exfiltrated\n",
                }),
            )
            .await
            .unwrap();
        assert!(
            !result.success,
            "ancestor-symlink escape MUST be refused, got: {}",
            result.output
        );
        assert!(
            result.output.contains("outside session scope"),
            "expected scope rejection, got: {}",
            result.output
        );
        // The escape file MUST NOT have been created at the symlink target.
        assert!(!outside_dir.path().join("leaked.txt").exists());
    }

    #[tokio::test]
    async fn write_file_falls_back_to_legacy_when_no_scope() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new(dir.path());
        let ctx = ToolContext::zero();
        assert!(ctx.session_scope.is_none());

        let ok = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "legacy.txt", "content": "legacy\n"}),
            )
            .await
            .unwrap();
        assert!(ok.success);
        assert!(dir.path().join("legacy.txt").exists());

        let bad = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "../escape.txt", "content": "bad"}),
            )
            .await
            .unwrap();
        assert!(!bad.success);
        assert!(bad.output.contains("outside working directory"));
    }

    // -----------------------------------------------------------------------
    // #1774: post-edit formatting integration.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_not_append_note_for_non_code_file_when_formatting_enabled() {
        // Deterministic (no formatter binary involved): a .txt file has no
        // mapped formatter, so even with the flag ON the output carries no
        // note and the bytes are exactly as written.
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new(dir.path());
        let mut ctx = ToolContext::zero();
        ctx.format_after_edit = true;

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "notes.txt", "content": "plain  text\n"}),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert!(!result.output.contains("reformatted"));
        assert!(!result.output.contains("Note:"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
            "plain  text\n"
        );
    }

    #[tokio::test]
    async fn should_format_written_file_when_format_after_edit_enabled() {
        if !crate::format::binary_on_path("rustfmt") {
            eprintln!("skipping: rustfmt not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new(dir.path());
        let mut ctx = ToolContext::zero();
        ctx.format_after_edit = true;

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "gen.rs",
                    "content": "fn main(){let x=1;println!(\"{}\",x);}\n",
                }),
            )
            .await
            .unwrap();
        assert!(result.success, "write must succeed: {}", result.output);
        assert!(
            result.output.contains("reformatted"),
            "output must state the file was reformatted: {}",
            result.output
        );
        assert!(
            result.output.contains("fn main() {"),
            "output must echo the formatted content: {}",
            result.output
        );
        let on_disk = std::fs::read_to_string(dir.path().join("gen.rs")).unwrap();
        assert!(
            on_disk.contains("fn main() {"),
            "file must be rustfmt-formatted on disk: {on_disk}"
        );
    }

    // -----------------------------------------------------------------------
    // #1976: per-path write-grant enforcement.
    // -----------------------------------------------------------------------

    fn fenced_tool(dir: &std::path::Path, patterns: &[&str], create_only: bool) -> WriteFileTool {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        WriteFileTool::new(dir).with_write_grant(
            crate::tools::write_grant::WritePathGrant::new(&owned, create_only)
                .expect("test grant compiles"),
        )
    }

    #[tokio::test]
    async fn write_grant_allows_creating_allowlisted_file() {
        // Acceptance (#1976): a worker granted write:["exemplar.card"],
        // create_only CAN create exemplar.card via write_file.
        let dir = tempfile::tempdir().unwrap();
        let tool = fenced_tool(dir.path(), &["exemplar.card"], true);

        let result = tool
            .execute(&serde_json::json!({"path": "exemplar.card", "content": "v1\n"}))
            .await
            .unwrap();
        assert!(
            result.success,
            "granted create must pass: {}",
            result.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("exemplar.card")).unwrap(),
            "v1\n"
        );
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

    #[tokio::test]
    async fn write_grant_without_create_only_allows_overwrite_of_allowlisted() {
        let dir = tempfile::tempdir().unwrap();
        let tool = fenced_tool(dir.path(), &["exemplar.card"], false);
        for content in ["v1\n", "v2\n"] {
            let result = tool
                .execute(&serde_json::json!({"path": "exemplar.card", "content": content}))
                .await
                .unwrap();
            assert!(result.success, "{}", result.output);
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("exemplar.card")).unwrap(),
            "v2\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_grant_symlinked_ancestor_cannot_reach_allowlisted_name() {
        // Acceptance (#1976): CANNOT bypass via symlink — `cards` symlinked
        // outside the workspace makes `cards/a.card` (lexically allowlisted)
        // resolve outside; the canonical match denies it.
        use std::os::unix::fs::symlink;
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), ws.path().join("cards")).unwrap();

        let tool = fenced_tool(ws.path(), &["cards/*.card"], false);
        let result = tool
            .execute(&serde_json::json!({"path": "cards/a.card", "content": "leak\n"}))
            .await
            .unwrap();
        assert!(!result.success, "symlink bypass must be refused");
        assert!(
            !outside.path().join("a.card").exists(),
            "nothing may land at the symlink target"
        );
    }

    #[tokio::test]
    async fn write_grant_skips_workspace_git_snapshot() {
        // #1976 security round 2 (codex): a fenced write to an allowlisted
        // sites/<slug>/index.html must NOT trigger workspace-git snapshotting,
        // which lexically derives repo root sites/<slug>/ and creates
        // .gitignore/.git there — NON-granted paths (and reopens the
        // ancestor-swap window). Assert the snapshot was skipped.
        let dir = tempfile::tempdir().unwrap();
        let tool = fenced_tool(dir.path(), &["sites/demo/index.html"], false);
        let result = tool
            .execute(&serde_json::json!({
                "path": "sites/demo/index.html",
                "content": "<h1>hi</h1>\n",
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "granted create must pass: {}",
            result.output
        );
        assert!(dir.path().join("sites/demo/index.html").exists());
        assert!(
            !dir.path().join("sites/demo/.git").exists(),
            "fence must skip git init",
        );
        assert!(
            !dir.path().join("sites/demo/.gitignore").exists(),
            "fence must skip .gitignore creation",
        );

        // Control: the SAME shape WITHOUT a fence DOES snapshot (writes
        // .gitignore at the derived repo root before git init), so the skip
        // assertion above is not vacuous.
        let unfenced = WriteFileTool::new(dir.path());
        let ctrl = unfenced
            .execute(&serde_json::json!({
                "path": "sites/other/index.html",
                "content": "<h1>c</h1>\n",
            }))
            .await
            .unwrap();
        assert!(ctrl.success, "{}", ctrl.output);
        assert!(
            dir.path().join("sites/other/.gitignore").exists(),
            "unfenced write must snapshot (control) — else the skip is vacuous",
        );
    }

    #[tokio::test]
    async fn write_grant_skips_post_edit_formatting() {
        // #1976 security round 2: format-under-fence is a no-op — the external
        // formatter (which re-resolves the lexical path) is skipped even with
        // format_after_edit ON, and the bytes stay exactly as written.
        let dir = tempfile::tempdir().unwrap();
        let tool = fenced_tool(dir.path(), &["gen.rs"], false);
        let mut ctx = ToolContext::zero();
        ctx.format_after_edit = true;

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "gen.rs", "content": "fn main(){let x=1;}\n"}),
            )
            .await
            .unwrap();
        assert!(result.success, "{}", result.output);
        assert!(
            !result.output.contains("reformatted") && !result.output.contains("Note:"),
            "no formatter must run under a fence: {}",
            result.output,
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("gen.rs")).unwrap(),
            "fn main(){let x=1;}\n",
            "fenced write must leave the bytes unformatted",
        );
    }

    #[tokio::test]
    async fn should_write_file_tool_invalidate_cache_after_write() {
        use crate::file_state_cache::{CacheEntry, FileStateCache};
        use std::sync::Arc;
        use std::time::SystemTime;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("note.txt");

        // Pre-populate the cache as if the file had been read already.
        let cache = Arc::new(FileStateCache::new());
        cache.put(CacheEntry::new(
            file_path.clone(),
            SystemTime::now(),
            0xABCD,
            42,
            false,
            None,
        ));
        assert_eq!(cache.len(), 1);

        // Wire the cache into the tool context.
        let mut ctx = ToolContext::zero();
        ctx.file_state_cache = Some(cache.clone());

        let tool = WriteFileTool::new(dir.path());
        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "note.txt", "content": "new body\n"}),
            )
            .await
            .unwrap();

        assert!(result.success);
        // After a successful write, the cache entry for this path must be gone.
        assert!(
            cache.peek(&file_path).is_none(),
            "write_file must invalidate the cached entry"
        );
        assert_eq!(cache.len(), 0);
    }

    // -----------------------------------------------------------------------
    // #1638 (c): the fail-closed overwrite guard, armed with windowed reads.
    // Both tools are armed per instance (never process-globally); every test
    // asserts on files it created itself, under its own session keys.
    // -----------------------------------------------------------------------

    use crate::tools::read_file::ReadFileTool;

    /// 1500 x 100-byte lines, 151,500 content bytes — well over the 48KiB
    /// window; line mode pages it at ~450 lines per page when armed.
    fn big_rows(dir: &std::path::Path, name: &str) -> String {
        let content = (1..=1500)
            .map(|i| format!("row {i:06}{}", "z".repeat(90)))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join(name), &content).unwrap();
        content
    }

    /// 700 x 100-byte lines, ~70,700 content bytes — over-window, but only
    /// two pages, for tests that page through repeatedly.
    fn medium_rows(dir: &std::path::Path, name: &str) {
        let content = (1..=700)
            .map(|i| format!("row {i:06}{}", "z".repeat(90)))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join(name), &content).unwrap();
    }

    fn ctx_with_session(session: &str) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.parent_session_key = Some(session.to_string());
        ctx
    }

    /// Follow the window footers from `offset` until the file has no
    /// continuation left — exactly what the refusal tells the model to do.
    async fn page_through(read: &ReadFileTool, ctx: &ToolContext, path: &str, mut next: usize) {
        for _ in 0..12 {
            let page = read
                .execute_with_context(ctx, &serde_json::json!({"path": path, "offset": next}))
                .await
                .unwrap();
            assert!(page.success, "{}", page.output);
            match page
                .output
                .split("offset: ")
                .nth(1)
                .and_then(|rest| rest.split('.').next())
                .and_then(|n| n.parse::<usize>().ok())
            {
                Some(n) => next = n,
                None => return, // no continuation footer — EOF reached
            }
        }
        panic!("paging did not converge");
    }

    #[tokio::test]
    async fn should_refuse_whole_overwrite_after_partial_read_then_allow_after_paging_through() {
        // The slides workflow reads a script, reconstructs it whole, and
        // write_files it back — and its prompt permits ONLY
        // read_file/write_file for authoring, so the refusal advice must be
        // followable with read_file alone, and following it must unlock the
        // write.
        let dir = tempfile::tempdir().unwrap();
        let original = big_rows(dir.path(), "script.js");
        let read = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);
        let ctx = ToolContext::zero();

        // Windowed read: the model has seen page one only.
        let page1 = read
            .execute(&serde_json::json!({"path": "script.js"}))
            .await
            .unwrap();
        assert!(page1.success && page1.output.contains("showing lines 1-450 of 1500"));

        // Whole-file overwrite from that partial view must be refused...
        let refused = write
            .execute(&serde_json::json!({"path": "script.js", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(
            !refused.success,
            "overwriting from a partial view must be refused: {}",
            refused.output
        );
        assert!(
            refused.output.contains("[PARTIAL_VIEW_OVERWRITE]"),
            "the refusal must be typed: {}",
            refused.output
        );
        assert!(
            refused.output.contains("only part of"),
            "the refusal must say what the model has actually seen: {}",
            refused.output
        );
        assert!(
            refused.output.contains("read_file") && refused.output.contains("offset"),
            "the advice must be followable in a context that allows ONLY \
             read_file/write_file (the slides prompt forbids the patch \
             tools): {}",
            refused.output
        );
        assert!(
            !refused.output.contains("edit_file") && !refused.output.contains("apply_patch"),
            "naming forbidden tools deadlocks the slides flow: {}",
            refused.output
        );
        // ...and must not have touched the file.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("script.js")).unwrap(),
            original,
            "a refused overwrite must leave the file intact"
        );

        // Page through to EOF the way the refusal advises; the same write
        // must then pass.
        page_through(&read, &ctx, "script.js", 451).await;
        let allowed = write
            .execute(&serde_json::json!({"path": "script.js", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(
            allowed.success,
            "after paging through the file, the guard's own advice must \
             unlock the write: {}",
            allowed.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("script.js")).unwrap(),
            "rebuilt\n"
        );
    }

    #[tokio::test]
    async fn should_refuse_overwriting_a_big_never_read_file_when_armed() {
        // THE fail-closed default: no ledger entry for an over-window file
        // means REFUSE — closing the giant-first-line hole (where the
        // advice branch records nothing), the restart hole (empty ledger),
        // and the eviction hole, all of which were fail-open when "no entry"
        // meant "allow".
        let dir = tempfile::tempdir().unwrap();
        let original = big_rows(dir.path(), "unread.js");
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);

        let refused = write
            .execute(&serde_json::json!({"path": "unread.js", "content": "blind rebuild\n"}))
            .await
            .unwrap();
        assert!(
            !refused.success,
            "an over-window file never read this session must not be \
             overwritten: {}",
            refused.output
        );
        assert!(
            refused.output.contains("[PARTIAL_VIEW_OVERWRITE]")
                && refused.output.contains("have not read it in this session"),
            "the refusal must say WHY and what to do: {}",
            refused.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("unread.js")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn should_allow_overwriting_a_never_read_small_file_when_armed() {
        // A file at or under the byte window is returned whole by a single
        // unbounded read — there is no partial-view illusion to protect
        // against, so blind overwrite stays exactly today's semantics.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blind.txt"), "old\n").unwrap();
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);

        let w = write
            .execute(&serde_json::json!({"path": "blind.txt", "content": "new\n"}))
            .await
            .unwrap();
        assert!(w.success, "{}", w.output);
    }

    #[tokio::test]
    async fn should_not_earn_completeness_from_an_unarmed_read() {
        // An UNARMED read records nothing — so even a full-range unarmed
        // read must not vouch for an armed write later (evidence gathered
        // while the feature was off is not evidence).
        let dir = tempfile::tempdir().unwrap();
        big_rows(dir.path(), "untracked.js");
        let read = ReadFileTool::new(dir.path()); // unarmed
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);

        let full = read
            .execute(
                &serde_json::json!({"path": "untracked.js", "start_line": 1, "end_line": 1500}),
            )
            .await
            .unwrap();
        assert!(full.success);

        let refused = write
            .execute(&serde_json::json!({"path": "untracked.js", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(
            !refused.success && refused.output.contains("have not read it in this session"),
            "unarmed reads must leave no trace the armed guard would trust: {}",
            refused.output
        );
    }

    #[tokio::test]
    async fn should_refuse_when_the_file_changed_on_disk_after_a_complete_read() {
        // Epoch validation at WRITE time: coverage is only as good as the
        // generation it was read from. A different (mtime, size) on disk
        // means the model's COMPLETE view is of a dead generation.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("moving.txt"), "v1\n").unwrap();
        let read = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);

        let r = read
            .execute(&serde_json::json!({"path": "moving.txt"}))
            .await
            .unwrap();
        assert!(r.success && r.output.contains("v1"));

        // Replace the file with different-sized content — even an equal
        // mtime second cannot mask a size change.
        std::fs::write(dir.path().join("moving.txt"), "version-two-longer\n").unwrap();

        let refused = write
            .execute(&serde_json::json!({"path": "moving.txt", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(
            !refused.success && refused.output.contains("changed on disk"),
            "stale coverage must refuse with re-read advice: {}",
            refused.output
        );
    }

    #[tokio::test]
    async fn should_recover_after_a_shrink_between_pages() {
        // H6: a file that shrinks between pages must not leave a stale
        // refusal forever — a fresh read of the new generation resets
        // coverage and unlocks the write.
        let dir = tempfile::tempdir().unwrap();
        big_rows(dir.path(), "shrinky.txt");
        let read = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);

        let page1 = read
            .execute(&serde_json::json!({"path": "shrinky.txt"}))
            .await
            .unwrap();
        assert!(page1.success && page1.output.contains("showing lines 1-450 of 1500"));

        // The file shrinks to a single small line.
        std::fs::write(dir.path().join("shrinky.txt"), "tiny now\n").unwrap();

        let refused = write
            .execute(&serde_json::json!({"path": "shrinky.txt", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(
            !refused.success,
            "coverage of the old generation must not authorize a write: {}",
            refused.output
        );

        // A fresh read of the (now small) file completes in one call...
        let fresh = read
            .execute(&serde_json::json!({"path": "shrinky.txt"}))
            .await
            .unwrap();
        assert!(fresh.success && fresh.output.contains("tiny now"));

        // ...and the write goes through.
        let allowed = write
            .execute(&serde_json::json!({"path": "shrinky.txt", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(
            allowed.success,
            "a fresh read of the current generation must recover: {}",
            allowed.output
        );
    }

    #[tokio::test]
    async fn should_refuse_across_sessions_even_after_a_complete_read() {
        // The ledger is keyed by (session, path): session A's COMPLETE must
        // never authorize session B's overwrite of content B has not seen.
        let dir = tempfile::tempdir().unwrap();
        medium_rows(dir.path(), "shared.js");
        let read = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);
        let ctx_a = ctx_with_session("xsession-a");
        let ctx_b = ctx_with_session("xsession-b");

        // Session A pages the file through to completion.
        let page1 = read
            .execute_with_context(&ctx_a, &serde_json::json!({"path": "shared.js"}))
            .await
            .unwrap();
        assert!(page1.success, "{}", page1.output);
        page_through(&read, &ctx_a, "shared.js", 451).await;

        // Session B has seen nothing and must be refused.
        let refused = write
            .execute_with_context(
                &ctx_b,
                &serde_json::json!({"path": "shared.js", "content": "rebuilt by b\n"}),
            )
            .await
            .unwrap();
        assert!(
            !refused.success && refused.output.contains("have not read it in this session"),
            "another session's coverage is not this session's: {}",
            refused.output
        );

        // Session A's own write is allowed (positive control).
        let allowed = write
            .execute_with_context(
                &ctx_a,
                &serde_json::json!({"path": "shared.js", "content": "rebuilt by a\n"}),
            )
            .await
            .unwrap();
        assert!(allowed.success, "{}", allowed.output);
    }

    #[tokio::test]
    async fn should_refuse_after_a_restart_until_reread() {
        // Restart safety by construction: the in-memory ledger is empty
        // after a restart, and absence REFUSES over-window overwrites, so a
        // fresh process can never silently trust pre-restart coverage.
        // (Simulated per session rather than clearing the whole ledger — a
        // global clear would wipe parallel tests' entries mid-flight.)
        let dir = tempfile::tempdir().unwrap();
        medium_rows(dir.path(), "reboot.js");
        let read = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);
        let ctx = ctx_with_session("restart-sim");

        let page1 = read
            .execute_with_context(&ctx, &serde_json::json!({"path": "reboot.js"}))
            .await
            .unwrap();
        assert!(page1.success, "{}", page1.output);
        page_through(&read, &ctx, "reboot.js", 451).await;

        // "Restart": this session's ledger entries are gone.
        crate::tools::read_window::reset_session_for_test("restart-sim");

        let refused = write
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "reboot.js", "content": "rebuilt\n"}),
            )
            .await
            .unwrap();
        assert!(
            !refused.success && refused.output.contains("have not read it in this session"),
            "post-restart, the model must re-read before overwriting: {}",
            refused.output
        );

        // Re-reading re-earns the write.
        let again = read
            .execute_with_context(&ctx, &serde_json::json!({"path": "reboot.js"}))
            .await
            .unwrap();
        assert!(again.success, "{}", again.output);
        page_through(&read, &ctx, "reboot.js", 451).await;
        let allowed = write
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": "reboot.js", "content": "rebuilt\n"}),
            )
            .await
            .unwrap();
        assert!(allowed.success, "{}", allowed.output);
    }

    #[tokio::test]
    async fn should_refuse_when_redacted_content_never_reached_the_model() {
        // H1b: the ledger records what the TOOL returned, but the loop
        // sanitizer rewrites afterwards — a view it would alter never
        // reaches the model as-is, so it must record TAINTED and a
        // whole-file rewrite must refuse (the model would write redaction
        // placeholders over real content).
        let dir = tempfile::tempdir().unwrap();
        let secret = format!("before\n{}\nafter\n", "a".repeat(64)); // 64+ hex chars => redacted
        std::fs::write(dir.path().join("secrets.env"), &secret).unwrap();
        let read = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);

        // The whole file fits one window — coverage is FULL, and that must
        // still not count as a faithful view.
        let r = read
            .execute(&serde_json::json!({"path": "secrets.env"}))
            .await
            .unwrap();
        assert!(r.success, "{}", r.output);

        let refused = write
            .execute(&serde_json::json!({"path": "secrets.env", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(
            !refused.success && refused.output.contains("redacted from your view"),
            "full coverage of redacted bytes is not knowledge: {}",
            refused.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("secrets.env")).unwrap(),
            secret,
            "the refusal must leave the secret-bearing file intact"
        );
    }

    #[tokio::test]
    async fn should_complete_via_byte_paging_and_allow_the_write() {
        // The giant-FIRST-line family end to end: line mode can only advise,
        // the fail-closed default refuses the blind write, raw byte paging
        // earns completeness, and the write then passes.
        let dir = tempfile::tempdir().unwrap();
        let giant = "G".repeat(60_000);
        std::fs::write(dir.path().join("one_line.min.js"), &giant).unwrap();
        let read = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);

        // Line mode: advice only (and no ledger entry).
        let advice = read
            .execute(&serde_json::json!({"path": "one_line.min.js"}))
            .await
            .unwrap();
        assert!(advice.success, "{}", advice.output);
        assert!(
            advice.output.contains("byte_offset: 0"),
            "the advice names the byte-mode continuation: {}",
            advice.output
        );

        // Fail-closed: advice is not a view; the write refuses.
        let refused = write
            .execute(&serde_json::json!({"path": "one_line.min.js", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(
            !refused.success && refused.output.contains("have not read it in this session"),
            "a giant-first-line file with no real view must refuse — this \
             was the fail-open hole in the first draft: {}",
            refused.output
        );

        // Byte-page the whole line: two slabs.
        let mut next = 0usize;
        for _ in 0..4 {
            let slab = read
                .execute(&serde_json::json!({"path": "one_line.min.js", "byte_offset": next}))
                .await
                .unwrap();
            assert!(slab.success, "{}", slab.output);
            match slab
                .output
                .split("byte_offset: ")
                .nth(1)
                .and_then(|rest| rest.split('.').next())
                .and_then(|n| n.parse::<usize>().ok())
            {
                Some(n) => next = n,
                None => break, // no footer — EOF
            }
        }

        let allowed = write
            .execute(&serde_json::json!({"path": "one_line.min.js", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(
            allowed.success,
            "byte paging to EOF must earn the write: {}",
            allowed.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("one_line.min.js")).unwrap(),
            "rebuilt\n"
        );
    }

    #[tokio::test]
    async fn should_allow_a_second_overwrite_of_a_file_the_model_just_wrote() {
        // A successful write records the view COMPLETE at the post-write
        // epoch — merely forgetting the path (the redesign as first
        // proposed) would refuse the model's next overwrite of a big file
        // it authored one call ago.
        let dir = tempfile::tempdir().unwrap();
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);
        let big_content = format!("created big\n{}\n", "x".repeat(60_000));

        // Creating a new file is always allowed...
        let first = write
            .execute(&serde_json::json!({"path": "authored.txt", "content": big_content}))
            .await
            .unwrap();
        assert!(first.success, "{}", first.output);

        // ...and overwriting one's own just-written over-window content too.
        let second = write
            .execute(&serde_json::json!({"path": "authored.txt", "content": "second version\n"}))
            .await
            .unwrap();
        assert!(
            second.success,
            "the author of the current content must not be locked out: {}",
            second.output
        );
    }

    #[tokio::test]
    async fn should_allow_overwrite_after_a_complete_unbounded_read_when_armed() {
        // A file that fits the window is returned whole; overwriting it is
        // exactly as safe as before.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small.txt"), "one\ntwo\nthree\n").unwrap();
        let read = ReadFileTool::new(dir.path()).with_window_enforcement(true);
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);

        let r = read
            .execute(&serde_json::json!({"path": "small.txt"}))
            .await
            .unwrap();
        assert!(r.success && r.output.contains("three"));

        let w = write
            .execute(&serde_json::json!({"path": "small.txt", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(
            w.success,
            "a fully-seen file must remain overwritable: {}",
            w.output
        );
    }

    #[tokio::test]
    async fn should_allow_creating_a_new_file_when_armed() {
        let dir = tempfile::tempdir().unwrap();
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);

        let w = write
            .execute(&serde_json::json!({"path": "brand_new.txt", "content": "hello\n"}))
            .await
            .unwrap();
        assert!(
            w.success,
            "creating a new file is never a partial overwrite"
        );
        assert!(dir.path().join("brand_new.txt").exists());
    }

    #[tokio::test]
    async fn should_clamp_pathological_paths_in_the_refusal() {
        // H4: refusal messages interpolate the caller's path SPELLING, which
        // is unbounded — a 50KB spelling must not push the refusal past the
        // loop's output cap (a blind head/tail cut there would mangle the
        // advice).
        let dir = tempfile::tempdir().unwrap();
        big_rows(dir.path(), "deep.js");
        let write = WriteFileTool::new(dir.path()).with_window_enforcement(true);
        let pathological = format!("{}deep.js", "./".repeat(25_000));

        let refused = write
            .execute(&serde_json::json!({"path": pathological, "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(!refused.success, "{}", refused.output);
        assert!(
            refused.output.contains("[PARTIAL_VIEW_OVERWRITE]"),
            "still the typed refusal: {}",
            octos_core::truncated_utf8(&refused.output, 200, "...")
        );
        assert!(
            refused.output.len() <= octos_core::tool_output_limit("write_file"),
            "the refusal must clamp the path so the loop backstop cannot \
             mangle the advice: {} bytes",
            refused.output.len()
        );
    }

    #[tokio::test]
    async fn should_not_guard_when_unarmed_even_after_a_partial_read() {
        // Unarmed behaviour is byte-identical to before: a ranged read
        // followed by a whole overwrite goes through untouched.
        let dir = tempfile::tempdir().unwrap();
        big_rows(dir.path(), "dormant.txt");
        let read = ReadFileTool::new(dir.path());
        let write = WriteFileTool::new(dir.path());

        let r = read
            .execute(&serde_json::json!({"path": "dormant.txt", "start_line": 1, "end_line": 5}))
            .await
            .unwrap();
        assert!(r.success);

        let w = write
            .execute(&serde_json::json!({"path": "dormant.txt", "content": "rebuilt\n"}))
            .await
            .unwrap();
        assert!(w.success, "the unarmed path must not change: {}", w.output);
        assert!(w.output.contains("Successfully wrote"));
    }
}
