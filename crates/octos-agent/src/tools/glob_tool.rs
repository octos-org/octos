//! Glob tool for finding files by pattern.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;

use super::{Tool, ToolResult};

/// Tool for finding files matching a glob pattern.
pub struct GlobTool {
    /// Base directory for searches.
    base_dir: PathBuf,
}

impl GlobTool {
    /// Create a new glob tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GlobInput {
    /// Glob pattern to match (e.g., "**/*.rs", "src/*.py").
    pattern: String,
    /// Maximum number of results to return.
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    100
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern. Use ** for recursive matching. Examples: '**/*.rs' finds all Rust files, 'src/**/*.py' finds Python files in src."
    }

    fn tags(&self) -> &[&str] {
        &["search", "code"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match (e.g., '**/*.rs', 'src/*.py')"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 100)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: GlobInput =
            serde_json::from_value(args.clone()).wrap_err("invalid glob tool input")?;

        let base_dir = self.base_dir.clone();
        let pattern = input.pattern.clone();
        let limit = input.limit;

        // Reject absolute patterns and parent traversal
        if pattern.starts_with('/') || pattern.contains("..") {
            return Ok(ToolResult {
                output: "Absolute paths and '..' are not allowed in glob patterns".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // Run glob in blocking task
        let result = tokio::task::spawn_blocking(move || {
            let full_pattern = format!("{}/{}", base_dir.display(), pattern);

            let mut files: Vec<String> = Vec::new();

            match glob::glob(&full_pattern) {
                Ok(paths) => {
                    for entry in paths.take(limit) {
                        match entry {
                            Ok(path) => {
                                // Make path relative to base_dir if possible
                                let display_path = path
                                    .strip_prefix(&base_dir)
                                    .map(|p| p.to_path_buf())
                                    .unwrap_or(path);
                                files.push(display_path.display().to_string());
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "glob entry error");
                            }
                        }
                    }
                }
                Err(e) => {
                    return Err(eyre::eyre!("invalid glob pattern: {}", e));
                }
            }

            Ok::<_, eyre::Report>(files)
        })
        .await??;

        if result.is_empty() {
            Ok(ToolResult {
                output: format!("No files found matching pattern: {}", input.pattern),
                success: true,
                ..Default::default()
            })
        } else {
            let count = result.len();
            let output = format!("Found {} file(s):\n{}", count, result.join("\n"));
            Ok(ToolResult {
                output,
                success: true,
                ..Default::default()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_glob_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();

        let tool = GlobTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "*.rs"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("2 file(s)"));
        assert!(result.output.contains("a.rs"));
        assert!(result.output.contains("b.rs"));
        assert!(!result.output.contains("c.txt"));
    }

    #[tokio::test]
    async fn test_glob_recursive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/nested/mod.rs"), "").unwrap();

        let tool = GlobTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "**/*.rs"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("2 file(s)"));
    }

    #[tokio::test]
    async fn test_glob_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        let tool = GlobTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "*.xyz"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("No files found"));
    }

    #[tokio::test]
    async fn test_glob_rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = GlobTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "/etc/*.conf"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("not allowed"));
    }

    #[tokio::test]
    async fn test_glob_rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let tool = GlobTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "../../*.rs"}))
            .await
            .unwrap();

        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_glob_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("file{i}.txt")), "").unwrap();
        }

        let tool = GlobTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "*.txt", "limit": 3}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("3 file(s)"));
    }

    #[test]
    fn test_tool_metadata() {
        let tool = GlobTool::new("/tmp");
        assert_eq!(tool.name(), "glob");
        assert!(tool.tags().contains(&"search"));
    }
}
