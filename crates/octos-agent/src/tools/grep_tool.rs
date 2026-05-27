//! Grep tool for searching file contents.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use ignore::WalkBuilder;
use octos_core::{PathClassification, SessionScope};
use regex::RegexBuilder;
use serde::Deserialize;

use super::{Tool, ToolContext, ToolResult};

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
    /// Optional path under which to search. When omitted the tool
    /// searches the base directory (legacy) or the scope workspace
    /// (when a SessionScope is wired through `ToolContext`).
    #[serde(default)]
    path: Option<String>,
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
        "Search file contents using regex. Respects .gitignore. Use file_pattern to filter which files to search (e.g., '*.rs'). Use path to scope the search to a specific directory."
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
                "path": {
                    "type": "string",
                    "description": "Optional directory to search under (defaults to the working directory)"
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
        // PR-B: legacy entry point routes through the typed path with a
        // zero-value context so out-of-band callers still get the same
        // SessionScope-aware behaviour when no scope is wired.
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let input: GrepInput =
            serde_json::from_value(args.clone()).wrap_err("invalid grep tool input")?;

        // Reject file_pattern with absolute paths or traversal.
        if let Some(ref fp) = input.file_pattern {
            if fp.starts_with('/') || fp.contains("..") {
                return Ok(ToolResult {
                    output: "Absolute paths and '..' are not allowed in file patterns".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        }

        // Resolve the search root.
        //
        // PR-B: when a `SessionScope` is wired, an explicit `path`
        // input is validated against the scope; without an explicit
        // path the search anchors at `scope.workspace()`. When no
        // scope is wired we anchor at `self.base_dir` and (if a
        // `path` is given) join it relative — the legacy resolver
        // refuses traversal.
        let search_root = match ctx.session_scope.as_ref() {
            Some(scope) => match input.path.as_deref() {
                Some(p) => match super::resolve_path_for_session_scope_read(scope, p) {
                    Ok(root) => root,
                    Err(reason) => {
                        return Ok(ToolResult {
                            output: format!("{reason}: {p}"),
                            success: false,
                            ..Default::default()
                        });
                    }
                },
                None => scope.workspace().to_path_buf(),
            },
            None => match input.path.as_deref() {
                Some(p) => match super::resolve_path(&self.base_dir, p) {
                    Ok(root) => root,
                    Err(_) => {
                        return Ok(ToolResult {
                            output: format!("Path outside working directory: {p}"),
                            success: false,
                            ..Default::default()
                        });
                    }
                },
                None => self.base_dir.clone(),
            },
        };

        let scope = ctx.session_scope.clone();
        let pattern_str = input.pattern.clone();
        let file_pattern = input.file_pattern.clone();
        let limit = input.limit;
        let context = input.context;
        let ignore_case = input.ignore_case;

        // Run search in blocking task.
        let result = tokio::task::spawn_blocking(move || {
            run_grep(
                scope,
                search_root,
                pattern_str,
                file_pattern,
                limit,
                context,
                ignore_case,
            )
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

fn run_grep(
    scope: Option<Arc<SessionScope>>,
    search_root: PathBuf,
    pattern_str: String,
    file_pattern: Option<String>,
    limit: usize,
    context: usize,
    ignore_case: bool,
) -> Result<(Vec<String>, usize)> {
    // Compile regex.
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

    // Use ignore crate to respect .gitignore.
    let walker = WalkBuilder::new(&search_root)
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

        // Skip directories.
        if path.is_dir() {
            continue;
        }

        // PR-B: if a SessionScope is wired, drop any file the walker
        // surfaces whose canonical path classifies OutOfScope. This
        // closes the symlink-loop escape: a symlink inside the
        // skill_dir pointing at `/etc` would otherwise let the walker
        // read passwd; canonicalize-then-classify rejects it.
        if let Some(scope) = scope.as_ref() {
            if matches!(
                scope.classify_canonical_path(path),
                PathClassification::OutOfScope
            ) {
                continue;
            }
        }

        // Apply file pattern filter.
        if let Some(ref fp) = file_pattern {
            let file_name = path.file_name().unwrap_or_default().to_string_lossy();
            let pattern = glob::Pattern::new(fp);
            if let Ok(p) = pattern {
                if !p.matches(&file_name) {
                    continue;
                }
            }
        }

        // Read file.
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // Skip binary or unreadable files
        };

        let lines: Vec<&str> = content.lines().collect();

        // Search lines.
        for (line_num, line) in lines.iter().enumerate() {
            if match_count >= limit {
                break;
            }

            if regex.is_match(line) {
                match_count += 1;

                let rel_path = path.strip_prefix(&search_root).unwrap_or(path).display();

                if context > 0 {
                    // Include context lines.
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

    Ok((matches, match_count))
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

    // ------------------------------------------------------------------
    // PR-B: SessionScope integration tests for GrepTool.
    // ------------------------------------------------------------------

    fn ctx_with_scope(scope: SessionScope) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "grep-with-scope".to_string();
        ctx.session_scope = Some(Arc::new(scope));
        ctx
    }

    #[tokio::test]
    async fn grep_inside_skill_dir_finds_matches() {
        // With a SessionScope wired and `path` pointing inside the
        // registered skill_dir, the walker descends into the
        // skill_dir and returns matches.
        let workspace = tempfile::tempdir().unwrap();
        let skill = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(skill.path().join("docs")).unwrap();
        std::fs::write(
            skill.path().join("docs/intro.md"),
            "# SKILL DEMO\nuse octos here\n",
        )
        .unwrap();
        std::fs::write(
            skill.path().join("docs/usage.md"),
            "no relevant content here\n",
        )
        .unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![])
            .unwrap()
            .with_skill_read_zones(vec![skill.path().to_path_buf()])
            .unwrap();

        let tool = GrepTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "pattern": "octos",
                    "path": skill.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        assert!(
            result.output.contains("intro.md"),
            "expected hit in intro.md, got: {}",
            result.output
        );
        assert!(result.output.contains("octos"));
    }

    #[tokio::test]
    async fn grep_refuses_out_of_scope_path() {
        // An explicit path outside every declared zone is refused
        // without walking it (cheaper failure mode + no leakage).
        let workspace = tempfile::tempdir().unwrap();
        let skill = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "leaked\n").unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![])
            .unwrap()
            .with_skill_read_zones(vec![skill.path().to_path_buf()])
            .unwrap();

        let tool = GrepTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "pattern": "leaked",
                    "path": outside.path().to_string_lossy(),
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
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_symlink_escape_in_skill_dir_drops_results() {
        // A symlink inside the skill_dir pointing at /tmp/<outside>
        // is dropped by the per-entry canonicalize-then-classify
        // guard, so grep walking the skill_dir doesn't surface
        // matches from outside.
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let skill = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("smuggled.txt"), "needle in haystack\n").unwrap();

        // Legitimate content inside the skill_dir — should NOT match.
        std::fs::write(skill.path().join("README.md"), "no hits here\n").unwrap();
        // Symlink under skill_dir pointing at the outside dir.
        symlink(outside.path(), skill.path().join("escape")).unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![])
            .unwrap()
            .with_skill_read_zones(vec![skill.path().to_path_buf()])
            .unwrap();

        let tool = GrepTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "pattern": "needle",
                    "path": skill.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            result.output.contains("No matches"),
            "symlink-out-of-scope must not surface matches, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_falls_back_to_legacy_when_no_scope() {
        // No scope wired => pre-PR-B base_dir-anchored behaviour.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();

        let tool = GrepTool::new(dir.path());
        let ctx = ToolContext::zero();
        assert!(ctx.session_scope.is_none());

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"pattern": "hello"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello"));
    }
}
