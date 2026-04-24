//! Write file tool for creating new files.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tracing::warn;

use super::{ConcurrencyClass, Tool, ToolContext, ToolResult};

/// Tool for writing/creating files.
pub struct WriteFileTool {
    /// Base directory for resolving relative paths.
    base_dir: PathBuf,
}

impl WriteFileTool {
    /// Create a new write file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
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
