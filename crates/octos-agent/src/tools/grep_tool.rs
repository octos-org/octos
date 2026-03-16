//! Grep tool for searching file contents.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Deserialize;

use super::{Tool, ToolResult};

/// Tool for searching file contents with regex.
pub struct GrepTool {
    /// Base directory for searches.
    base_dir: PathBuf,
}

impl GrepTool {
    /// Create a new grep tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GrepInput {
    /// Regex pattern to search for.
    pattern: String,
    /// Optional glob pattern to filter files.
    #[serde(default)]
    file_pattern: Option<String>,
    /// Maximum number of matches to return.
    #[serde(default = "default_limit")]
    limit: usize,
    /// Include N lines of context around matches.
    #[serde(default)]
    context: usize,
    /// Case insensitive search.
    #[serde(default)]
    ignore_case: bool,
}

fn default_limit() -> usize {
    50
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents using regex. Respects .gitignore. Use file_pattern to filter which files to search (e.g., '*.rs')."
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
                    "description": "Regex pattern to search for"
                },
                "file_pattern": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g., '*.rs', '*.py')"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of matches (default: 50)"
                },
                "context": {
                    "type": "integer",
                    "description": "Lines of context around matches (default: 0)"
                },
                "ignore_case": {
                    "type": "boolean",
                    "description": "Case insensitive search (default: false)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: GrepInput =
            serde_json::from_value(args.clone()).wrap_err("invalid grep tool input")?;

        let base_dir = self.base_dir.clone();
        let pattern_str = input.pattern.clone();
        let file_pattern = input.file_pattern.clone();
        let limit = input.limit;
        let context = input.context;
        let ignore_case = input.ignore_case;

        // Reject file_pattern with absolute paths or traversal
        if let Some(ref fp) = file_pattern {
            if fp.starts_with('/') || fp.contains("..") {
                return Ok(ToolResult {
                    output: "Absolute paths and '..' are not allowed in file patterns".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        }

        // Run search in blocking task
        let result = tokio::task::spawn_blocking(move || {
            // Compile regex
            let regex_pattern = if ignore_case {
                format!("(?i){}", pattern_str)
            } else {
                pattern_str.clone()
            };

            let regex = RegexBuilder::new(&regex_pattern)
                .size_limit(1 << 20) // 1 MB compiled regex limit (prevents ReDoS)
                .build()
                .wrap_err_with(|| format!("invalid regex: {}", pattern_str))?;

            let mut matches: Vec<String> = Vec::new();
            let mut match_count = 0;

            // Use ignore crate to respect .gitignore
            let walker = WalkBuilder::new(&base_dir)
                .hidden(false)
                .git_ignore(true)
                .build();

            for entry in walker {
                if match_count >= limit {
                    break;
                }

                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                let path = entry.path();

                // Skip directories
                if path.is_dir() {
                    continue;
                }

                // Apply file pattern filter
                if let Some(ref fp) = file_pattern {
                    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
                    let pattern = glob::Pattern::new(fp);
                    if let Ok(p) = pattern {
                        if !p.matches(&file_name) {
                            continue;
                        }
                    }
                }

                // Read file
                let content = match std::fs::read_to_string(path) {
                    Ok(c) => c,
                    Err(_) => continue, // Skip binary or unreadable files
                };

                let lines: Vec<&str> = content.lines().collect();

                // Search lines
                for (line_num, line) in lines.iter().enumerate() {
                    if match_count >= limit {
                        break;
                    }

                    if regex.is_match(line) {
                        match_count += 1;

                        let rel_path = path.strip_prefix(&base_dir).unwrap_or(path).display();

                        if context > 0 {
                            // Include context lines
                            let start = line_num.saturating_sub(context);
                            let end = (line_num + context + 1).min(lines.len());

                            let mut ctx_output = format!("{}:\n", rel_path);
                            for (i, ctx_line) in lines[start..end].iter().enumerate() {
                                let actual_line = start + i;
                                let marker = if actual_line == line_num { ">" } else { " " };
                                ctx_output.push_str(&format!(
                                    "{} {:4}│ {}\n",
                                    marker,
                                    actual_line + 1,
                                    ctx_line
                                ));
                            }
                            matches.push(ctx_output);
                        } else {
                            matches.push(format!("{}:{}: {}", rel_path, line_num + 1, line.trim()));
                        }
                    }
                }
            }

            Ok::<_, eyre::Report>((matches, match_count))
        })
        .await??;

        let (matches, count) = result;

        if matches.is_empty() {
            Ok(ToolResult {
                output: format!("No matches found for pattern: {}", input.pattern),
                success: true,
                ..Default::default()
            })
        } else {
            let truncated = if count >= limit {
                format!(" (limited to {})", limit)
            } else {
                String::new()
            };
            let output = format!(
                "Found {} match(es){}:\n\n{}",
                count,
                truncated,
                matches.join("\n")
            );
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

    fn setup_project(dir: &std::path::Path) {
        std::fs::write(
            dir.join("main.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("lib.rs"),
            "pub fn greet() -> &'static str {\n    \"hello\"\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("readme.txt"), "This is a readme file.\n").unwrap();
    }

    #[tokio::test]
    async fn test_grep_finds_matches() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "hello"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("hello"));
        assert!(result.output.contains("match"));
    }

    #[tokio::test]
    async fn test_grep_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "nonexistent_string_xyz"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("No matches"));
    }

    #[tokio::test]
    async fn test_grep_with_file_pattern() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "hello", "file_pattern": "*.rs"}))
            .await
            .unwrap();

        assert!(result.success);
        // Should find matches in .rs files but not readme.txt
        assert!(!result.output.contains("readme.txt"));
    }

    #[tokio::test]
    async fn test_grep_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            "Hello World\nhello world\nHELLO WORLD\n",
        )
        .unwrap();

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "hello", "ignore_case": true}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("3 match"));
    }

    #[tokio::test]
    async fn test_grep_with_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ctx.txt"), "before\ntarget line\nafter\n").unwrap();

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "target", "context": 1}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("before"));
        assert!(result.output.contains("target line"));
        assert!(result.output.contains("after"));
    }

    #[tokio::test]
    async fn test_grep_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (0..20).map(|i| format!("match line {i}\n")).collect();
        std::fs::write(dir.path().join("many.txt"), &content).unwrap();

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "match", "limit": 5}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("5 match"));
        assert!(result.output.contains("limited to 5"));
    }

    #[tokio::test]
    async fn test_grep_rejects_traversal_in_file_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "test", "file_pattern": "../../*.txt"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("not allowed"));
    }

    #[tokio::test]
    async fn test_grep_invalid_regex() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "data").unwrap();

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "[invalid"}))
            .await;

        assert!(result.is_err());
    }

    #[test]
    fn test_tool_metadata() {
        let tool = GrepTool::new("/tmp");
        assert_eq!(tool.name(), "grep");
        assert!(tool.tags().contains(&"search"));
    }
}
