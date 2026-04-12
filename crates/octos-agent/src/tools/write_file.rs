//! Write file tool for creating new files.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tracing::warn;

use super::{Tool, ToolResult};

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
}
