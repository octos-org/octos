//! Write file tool for creating new files.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tracing::warn;

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
}

impl WriteFileTool {
    /// Create a new write file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadWrite,
        }
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
}

#[derive(Debug, Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, or overwrites if it does."
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
                    "description": "Path to the file to write"
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
            serde_json::from_value(args.clone()).wrap_err("invalid write_file tool input")?;

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

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .wrap_err_with(|| format!("failed to create directories: {}", parent.display()))?;
        }

        // Write file (O_NOFOLLOW atomically rejects symlinks, no TOCTOU race)
        if let Err(e) = super::write_no_follow(&path, input.content.as_bytes()).await {
            return Ok(super::file_io_error(e, &input.path));
        }

        // M8.4: invalidate any stale cache entry for this path — the file's
        // contents (and mtime) just changed, so previous reads must not serve
        // a [FILE_UNCHANGED] stub on the next read.
        if let Some(cache) = ctx.file_state_cache.as_ref() {
            cache.invalidate(&path);
        }

        if let Err(error) =
            crate::workspace_git::snapshot_workspace_change(&self.base_dir, &path, "write_file")
        {
            warn!(
                path = %input.path,
                error = %error,
                "workspace git snapshot failed after write_file"
            );
        }

        let line_count = input.content.lines().count();
        Ok(ToolResult {
            output: format!("Successfully wrote {} lines to {}", line_count, input.path),
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
}
