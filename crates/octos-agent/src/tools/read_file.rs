//! Read file tool.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;

use super::{Tool, ToolContext, ToolResult};

/// Tool for reading file contents.
pub struct ReadFileTool {
    /// Base directory for resolving relative paths.
    base_dir: PathBuf,
}

impl ReadFileTool {
    /// Create a new read file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReadFileInput {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns the file content with line numbers."
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
                    "description": "Path to the file to read (relative to working directory)"
                },
                "start_line": {
                    "type": "integer",
                    "description": "Optional starting line number (1-indexed)"
                },
                "end_line": {
                    "type": "integer",
                    "description": "Optional ending line number (1-indexed, inclusive)"
                }
            },
            "required": ["path"]
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
            serde_json::from_value(args.clone()).wrap_err("invalid read_file tool input")?;

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

        // Reject files larger than 10MB to prevent OOM (output is capped to 100KB
        // anyway, and reading a multi-GB file just to slice a few lines is wasteful).
        const MAX_FILE_BYTES: u64 = 10_000_000;
        match tokio::fs::metadata(&path).await {
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
            _ => {}
        }

        // Read file (O_NOFOLLOW atomically rejects symlinks, no TOCTOU race)
        let content = match super::read_no_follow(&path).await {
            Ok(c) => c,
            Err(e) => return Ok(super::file_io_error(e, &input.path)),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Apply line range
        let start = input.start_line.unwrap_or(1).saturating_sub(1);
        let end = input.end_line.unwrap_or(total_lines).min(total_lines);

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

        // Format with line numbers
        let mut output = String::new();
        let line_num_width = end.to_string().len();

        for (idx, line) in lines[start..end].iter().enumerate() {
            let line_num = start + idx + 1;
            output.push_str(&format!(
                "{:>width$}│ {}\n",
                line_num,
                line,
                width = line_num_width
            ));
        }

        // Add file info
        if start > 0 || end < total_lines {
            output.push_str(&format!(
                "\n(showing lines {}-{} of {})",
                start + 1,
                end,
                total_lines
            ));
        }

        // Truncate if too long
        const MAX_OUTPUT: usize = 100000;
        octos_core::truncate_utf8(&mut output, MAX_OUTPUT, "\n... (content truncated)");

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
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
}
