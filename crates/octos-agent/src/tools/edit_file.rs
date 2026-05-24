//! Edit file tool for making precise text replacements.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tracing::warn;

use super::{ConcurrencyClass, Tool, ToolContext, ToolResult};
use crate::policy::{FileAccessMode, FilesystemScope};

/// Tool for editing files via string replacement.
pub struct EditFileTool {
    /// Base directory for resolving relative paths.
    base_dir: PathBuf,
    /// Effective filesystem scope.
    filesystem_scope: FilesystemScope,
    /// Whether writes are permitted.
    file_access: FileAccessMode,
}

impl EditFileTool {
    /// Create a new edit file tool.
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
struct EditFileInput {
    path: String,
    old_string: String,
    new_string: String,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact string with a new string. The old_string must match exactly (including whitespace and indentation)."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        // edit_file rewrites a file in place — same race hazard as
        // write_file. Serialize the whole batch. See M8.8.
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
                "old_string": {
                    "type": "string",
                    "description": "The exact string to find and replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "The string to replace it with"
                }
            },
            "required": ["path", "old_string", "new_string"]
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
        let input: EditFileInput =
            serde_json::from_value(args.clone()).wrap_err("invalid edit_file tool input")?;

        if !self.file_access.allows_write() {
            return Ok(ToolResult {
                output: "edit_file is not permitted by read-only filesystem access".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // Phase 2-C of the SessionScope migration: when the host has
        // threaded a scope through `ToolContext`, use it as the single
        // source of truth for base_dir + path classification. Same
        // write policy as `write_file` — `InWorkspace` and
        // `InGrantedDir` allowed; `InSharedZone` and `OutOfScope`
        // refused. The shared helper canonicalizes the candidate before
        // classification so ancestor symlinks can't smuggle an edit
        // out of the workspace.
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

        // Read current content (O_NOFOLLOW atomically rejects symlinks)
        let content = match super::read_no_follow(&path).await {
            Ok(c) => c,
            Err(e) => return Ok(super::file_io_error(e, &input.path)),
        };

        // Check if old_string exists
        let count = content.matches(&input.old_string).count();

        if count == 0 {
            return Ok(ToolResult {
                output: format!(
                    "String not found in file. Make sure the old_string matches exactly.\n\nSearched for:\n```\n{}\n```",
                    input.old_string
                ),
                success: false,
                ..Default::default()
            });
        }

        if count > 1 {
            return Ok(ToolResult {
                output: format!(
                    "Found {} occurrences of the string. Please provide more context to make the match unique.",
                    count
                ),
                success: false,
                ..Default::default()
            });
        }

        // Perform replacement
        let new_content = content.replacen(&input.old_string, &input.new_string, 1);

        // Write back (O_NOFOLLOW)
        if let Err(e) = super::write_no_follow(&path, new_content.as_bytes()).await {
            return Ok(super::file_io_error(e, &input.path));
        }

        // M8.4: invalidate any stale cache entry — the file's contents and
        // mtime just changed.
        if let Some(cache) = ctx.file_state_cache.as_ref() {
            cache.invalidate(&path);
        }

        if let Err(error) =
            crate::workspace_git::snapshot_workspace_change(&self.base_dir, &path, "edit_file")
        {
            warn!(
                path = %input.path,
                error = %error,
                "workspace git snapshot failed after edit_file"
            );
        }

        Ok(ToolResult {
            output: format!("Successfully edited {}", input.path),
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
    fn edit_file_tool_is_exclusive() {
        // edit_file rewrites the target file; parallel read_file would race
        // on in-flight content, so it must serialize (M8.8).
        let dir = tempfile::tempdir().unwrap();
        let tool = EditFileTool::new(dir.path());
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Exclusive);
    }

    #[tokio::test]
    async fn test_edit_file_basic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "code.rs",
                "old_string": "println!(\"hello\")",
                "new_string": "println!(\"world\")"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let content = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert!(content.contains("println!(\"world\")"));
        assert!(!content.contains("println!(\"hello\")"));
    }

    #[tokio::test]
    async fn test_edit_file_string_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "some content").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "file.txt",
                "old_string": "nonexistent string",
                "new_string": "replacement"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("String not found"));
    }

    #[tokio::test]
    async fn test_edit_file_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dup.txt"), "foo bar foo baz foo").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "dup.txt",
                "old_string": "foo",
                "new_string": "qux"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("3 occurrences"));
    }

    #[tokio::test]
    async fn test_edit_file_multiline_replacement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("multi.txt"), "line1\nline2\nline3\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "multi.txt",
                "old_string": "line2\nline3",
                "new_string": "replaced2\nreplaced3"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let content = std::fs::read_to_string(dir.path().join("multi.txt")).unwrap();
        assert!(content.contains("replaced2\nreplaced3"));
    }

    #[tokio::test]
    async fn test_edit_file_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EditFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({
                "path": "nope.txt",
                "old_string": "a",
                "new_string": "b"
            }))
            .await
            .unwrap();

        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_edit_file_traversal_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EditFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({
                "path": "../../etc/passwd",
                "old_string": "root",
                "new_string": "hacked"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("outside working directory"));
    }

    #[test]
    fn test_tool_metadata() {
        let tool = EditFileTool::new("/tmp");
        assert_eq!(tool.name(), "edit_file");
        assert!(tool.tags().contains(&"fs"));
    }

    // -----------------------------------------------------------------------
    // Phase 2-C: SessionScope integration tests for EditFileTool.
    // -----------------------------------------------------------------------

    use octos_core::SessionScope;
    use std::sync::Arc;

    fn ctx_with_scope(scope: SessionScope) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "edit-with-scope".to_string();
        ctx.session_scope = Some(Arc::new(scope));
        ctx
    }

    #[tokio::test]
    async fn edit_file_uses_scope_workspace_as_base_dir_for_relative_paths() {
        // Relative edit path anchors at `scope.workspace()`, not the
        // legacy `base_dir`. Pre-create the target file there.
        let scope_dir = tempfile::tempdir().unwrap();
        let legacy_dir = tempfile::tempdir().unwrap();
        std::fs::write(scope_dir.path().join("doc.md"), "before\n").unwrap();
        std::fs::write(legacy_dir.path().join("doc.md"), "decoy\n").unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = EditFileTool::new(legacy_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "doc.md",
                    "old_string": "before",
                    "new_string": "after",
                }),
            )
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);

        // Only the scope-dir copy is mutated; the legacy decoy is
        // untouched. (Edit even refused to look at the legacy file.)
        assert_eq!(
            std::fs::read_to_string(scope_dir.path().join("doc.md")).unwrap(),
            "after\n",
        );
        assert_eq!(
            std::fs::read_to_string(legacy_dir.path().join("doc.md")).unwrap(),
            "decoy\n",
        );
    }

    #[tokio::test]
    async fn edit_file_refuses_out_of_scope_path() {
        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("target.txt");
        std::fs::write(&outside_file, "untouched\n").unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = EditFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": outside_file.to_string_lossy(),
                    "old_string": "untouched",
                    "new_string": "owned",
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
        // File MUST remain unchanged.
        assert_eq!(
            std::fs::read_to_string(&outside_file).unwrap(),
            "untouched\n"
        );
    }

    #[tokio::test]
    async fn edit_file_allows_in_workspace_path() {
        let scope_dir = tempfile::tempdir().unwrap();
        std::fs::write(scope_dir.path().join("inside.txt"), "alpha\n").unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = EditFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "inside.txt",
                    "old_string": "alpha",
                    "new_string": "beta",
                }),
            )
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        assert_eq!(
            std::fs::read_to_string(scope_dir.path().join("inside.txt")).unwrap(),
            "beta\n",
        );
    }

    #[tokio::test]
    async fn edit_file_refuses_write_to_shared_zone() {
        // Multi-tenant shared zones are read-only for session workers
        // — symmetric with `write_file_refuses_write_to_shared_zone`.
        let data_dir = tempfile::tempdir().unwrap();
        let data = data_dir.path().to_path_buf();
        std::fs::create_dir_all(data.join("research/topic")).unwrap();
        std::fs::create_dir_all(data.join("users/web-1/workspace")).unwrap();
        let shared_file = data.join("research/topic/notes.md");
        std::fs::write(&shared_file, "untouched\n").unwrap();

        let scope = SessionScope::multi_tenant_with_default_zones(
            data.clone(),
            "dspfac".into(),
            "web-1".into(),
        )
        .unwrap();
        let tool = EditFileTool::new(scope.workspace());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": shared_file.to_string_lossy(),
                    "old_string": "untouched",
                    "new_string": "owned",
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
        assert_eq!(
            std::fs::read_to_string(&shared_file).unwrap(),
            "untouched\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_refuses_ancestor_symlink_escape() {
        // Symmetric with the write_file ancestor-symlink test. Even
        // with a real target file pre-staged at the symlink target,
        // the scoped resolver must refuse before O_NOFOLLOW would
        // (correctly) bail on the symlink itself.
        use std::os::unix::fs::symlink;

        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        std::fs::write(outside_dir.path().join("target.txt"), "secret\n").unwrap();
        let link_path = scope_dir.path().join("link");
        symlink(outside_dir.path(), &link_path).unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = EditFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "link/target.txt",
                    "old_string": "secret",
                    "new_string": "owned",
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
        // Real file at the symlink target must remain unchanged.
        assert_eq!(
            std::fs::read_to_string(outside_dir.path().join("target.txt")).unwrap(),
            "secret\n",
        );
    }

    #[tokio::test]
    async fn edit_file_falls_back_to_legacy_when_no_scope() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), "x\n").unwrap();
        let tool = EditFileTool::new(dir.path());
        let ctx = ToolContext::zero();
        assert!(ctx.session_scope.is_none());

        let ok = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "ok.txt",
                    "old_string": "x",
                    "new_string": "y",
                }),
            )
            .await
            .unwrap();
        assert!(ok.success);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ok.txt")).unwrap(),
            "y\n"
        );

        let bad = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "../escape.txt",
                    "old_string": "a",
                    "new_string": "b",
                }),
            )
            .await
            .unwrap();
        assert!(!bad.success);
        assert!(bad.output.contains("outside working directory"));
    }

    #[tokio::test]
    async fn should_edit_file_tool_invalidate_cache_after_edit() {
        use crate::file_state_cache::{CacheEntry, FileStateCache};
        use std::sync::Arc;
        use std::time::SystemTime;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("code.rs");
        std::fs::write(&file_path, "fn foo() {}\n").unwrap();

        let cache = Arc::new(FileStateCache::new());
        cache.put(CacheEntry::new(
            file_path.clone(),
            SystemTime::now(),
            0xCAFE,
            12,
            false,
            None,
        ));
        assert_eq!(cache.len(), 1);

        let mut ctx = ToolContext::zero();
        ctx.file_state_cache = Some(cache.clone());

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "code.rs",
                    "old_string": "fn foo() {}",
                    "new_string": "fn bar() {}"
                }),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert!(cache.peek(&file_path).is_none());
    }
}
