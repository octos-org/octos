//! List directory tool.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{Tool, ToolResult};

/// List contents of a directory.
pub struct ListDirTool {
    base_dir: PathBuf,
}

impl ListDirTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    path: String,
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List the contents of a directory."
    }

    fn tags(&self) -> &[&str] {
        &["search", "code"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The directory path to list (relative to working directory)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input = serde_json::from_value(args.clone())?;

        let target = match super::resolve_path(&self.base_dir, &input.path) {
            Ok(p) => p,
            Err(_) => {
                return Ok(ToolResult {
                    output: format!("Path outside working directory: {}", input.path),
                    success: false,
                    ..Default::default()
                });
            }
        };

        if let Some(r) = super::reject_symlink(&target).await {
            return Ok(r);
        }

        if !target.exists() {
            return Ok(ToolResult {
                output: format!("Error: Directory not found: {}", input.path),
                success: false,
                ..Default::default()
            });
        }

        if !target.is_dir() {
            return Ok(ToolResult {
                output: format!("Error: Not a directory: {}", input.path),
                success: false,
                ..Default::default()
            });
        }

        let mut entries = match tokio::fs::read_dir(&target).await {
            Ok(entries) => entries,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Error: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }

        dirs.sort();
        files.sort();

        if dirs.is_empty() && files.is_empty() {
            return Ok(ToolResult {
                output: format!("Directory {} is empty.", input.path),
                success: true,
                ..Default::default()
            });
        }

        let mut out = String::new();
        for d in &dirs {
            out.push_str(&format!("[dir]  {d}\n"));
        }
        for f in &files {
            out.push_str(&format!("[file] {f}\n"));
        }

        Ok(ToolResult {
            output: format!(
                "{} entries in {}:\n{}",
                dirs.len() + files.len(),
                input.path,
                out.trim_end()
            ),
            success: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_list_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("file.txt"), "hello").unwrap();

        let tool = ListDirTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "."}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("[dir]  subdir"));
        assert!(result.output.contains("[file] file.txt"));
    }

    #[tokio::test]
    async fn test_not_found() {
        let dir = TempDir::new().unwrap();
        let tool = ListDirTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "nonexistent"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("not found"));
    }
}
