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
    /// #1767: `filePath` is the industry-convention alias.
    #[serde(alias = "filePath")]
    path: String,
    #[serde(alias = "oldString")]
    old_string: String,
    #[serde(alias = "newString")]
    new_string: String,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing a string with a new string. An exact match of old_string is preferred; when none exists, whitespace-, indentation- and escape-tolerant fuzzy matching is tried as a fallback. The old_string must identify a single location."
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
                    "description": "Path to the file to edit (alias: filePath)"
                },
                "old_string": {
                    "type": "string",
                    "description": "The string to find and replace. An exact match is preferred; minor whitespace/indentation/escape differences are tolerated as a fallback. Must identify a unique location. (alias: oldString)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The string to replace it with (alias: newString)"
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

        if input.old_string.is_empty() {
            return Ok(ToolResult {
                output: "old_string must not be empty".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // #1771: cascading replacer chain — exact match first, then
        // increasingly whitespace/indentation/escape-tolerant fallbacks.
        let (range, replacer_name) = match super::replacer::find_replacement(
            &content,
            &input.old_string,
        ) {
            super::replacer::ChainOutcome::Match { range, replacer } => (range, replacer),
            super::replacer::ChainOutcome::Ambiguous { count, replacer } => {
                return Ok(ToolResult {
                    output: format!(
                        "Found {count} occurrences of the string (via {replacer} replacer). Please provide more context to make the match unique.",
                    ),
                    success: false,
                    ..Default::default()
                });
            }
            super::replacer::ChainOutcome::NoMatch => {
                return Ok(ToolResult {
                    output: format!(
                        "String not found in file. No exact match, and no fuzzy match via the line-trimmed, whitespace-normalized, indentation-flexible, escape-normalized or block-anchor replacers.\n\nSearched for:\n```\n{}\n```",
                        input.old_string
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // Safety guard: a fuzzy matcher must never silently swallow far more
        // of the file than the old_string described.
        let matched_text = &content[range.clone()];
        if super::replacer::is_disproportionate_match(matched_text, &input.old_string) {
            return Ok(ToolResult {
                output: format!(
                    "Fuzzy match rejected as disproportionate: the {replacer_name} replacer matched {} lines / {} bytes for an old_string of {} lines / {} bytes. Provide more context so the match is precise.",
                    matched_text.lines().count(),
                    matched_text.len(),
                    input.old_string.lines().count(),
                    input.old_string.len(),
                ),
                success: false,
                ..Default::default()
            });
        }

        if replacer_name != "exact" {
            tracing::info!(
                replacer = replacer_name,
                path = %input.path,
                "edit_file fuzzy match succeeded"
            );
        }

        // Perform replacement by byte range — the fuzzy-matched span may
        // occur elsewhere as a plain substring, so a string replacen could
        // hit the wrong occurrence.
        let mut new_content =
            String::with_capacity(content.len() - range.len() + input.new_string.len());
        new_content.push_str(&content[..range.start]);
        new_content.push_str(&input.new_string);
        new_content.push_str(&content[range.end..]);

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

        // Report which replacer produced the match. The exact-match wording
        // is kept identical to the historical output for compatibility.
        let output = if replacer_name == "exact" {
            format!("Successfully edited {}", input.path)
        } else {
            format!(
                "Successfully edited {} (fuzzy match via {replacer_name} replacer)",
                input.path
            )
        };

        Ok(ToolResult {
            output,
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

    // -----------------------------------------------------------------------
    // #1771: cascading fuzzy replacer chain.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_match_via_line_trimmed_when_indentation_differs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    if ready {\n        launch();\n    }\n}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        // LLM lost the indentation (flush left) but every line's trimmed
        // content is right — the line_trimmed replacer must recover it.
        let result = tool
            .execute(&serde_json::json!({
                "path": "code.rs",
                "old_string": "if ready {\nlaunch();\n}",
                "new_string": "if ready {\n        abort();\n    }"
            }))
            .await
            .unwrap();

        assert!(
            result.success,
            "line-trimmed fuzzy match should succeed: {}",
            result.output
        );
        assert!(
            result.output.contains("line_trimmed"),
            "success output must report which replacer matched: {}",
            result.output
        );
        let content = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert!(content.contains("abort();"));
        assert!(!content.contains("launch();"));
    }

    #[tokio::test]
    async fn should_match_via_whitespace_normalized_when_internal_runs_differ() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("w.rs"), "let x  =  compute( a, b );\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "w.rs",
                "old_string": "let x = compute( a, b );",
                "new_string": "let x = compute(a, b, c);"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("whitespace_normalized"));
        let content = std::fs::read_to_string(dir.path().join("w.rs")).unwrap();
        assert_eq!(content, "let x = compute(a, b, c);\n");
    }

    #[tokio::test]
    async fn should_match_via_indentation_flexible_when_blank_boundary_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("i.rs"),
            "fn wrapper() {\n    step_one();\n    step_two();\n}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        // Needle copied with stray blank lines around the block and a
        // uniformly deeper indent — only the dedent matcher recovers it.
        let result = tool
            .execute(&serde_json::json!({
                "path": "i.rs",
                "old_string": "\n        step_one();\n        step_two();\n\n",
                "new_string": "    merged_steps();"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("indentation_flexible"));
        let content = std::fs::read_to_string(dir.path().join("i.rs")).unwrap();
        assert_eq!(content, "fn wrapper() {\n    merged_steps();\n}\n");
    }

    #[tokio::test]
    async fn should_match_via_escape_normalized_when_newline_double_escaped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("e.rs"), "alpha {\n    beta();\n}\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        // old_string carries a literal backslash-n instead of a newline.
        let result = tool
            .execute(&serde_json::json!({
                "path": "e.rs",
                "old_string": "alpha {\\n    beta();",
                "new_string": "alpha {\n    gamma();"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("escape_normalized"));
        let content = std::fs::read_to_string(dir.path().join("e.rs")).unwrap();
        assert_eq!(content, "alpha {\n    gamma();\n}\n");
    }

    #[tokio::test]
    async fn should_match_via_block_anchor_when_middle_line_drifted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("b.rs"),
            "fn compute() {\n    let total = base + extra;\n    total * 2\n}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        // Middle line remembered slightly wrong (offset vs extra) — first
        // and last lines anchor the block, similarity carries the middle.
        let result = tool
            .execute(&serde_json::json!({
                "path": "b.rs",
                "old_string": "fn compute() {\n    let total = base + offset;\n    total * 2\n}",
                "new_string": "fn compute() {\n    base * 3\n}"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("block_anchor"));
        let content = std::fs::read_to_string(dir.path().join("b.rs")).unwrap();
        assert_eq!(content, "fn compute() {\n    base * 3\n}\n");
    }

    #[tokio::test]
    async fn should_reject_disproportionate_block_anchor_span() {
        let dir = tempfile::tempdir().unwrap();
        let original = "fn f() {\n    a();\n    b();\n    c();\n    d();\n    e();\n    g();\n}\n";
        std::fs::write(dir.path().join("g.rs"), original).unwrap();

        let tool = EditFileTool::new(dir.path());
        // Anchors would bracket 8 lines for a 3-line old_string (> the
        // max(3+3, 3*2) = 6 line cap) — the guard must refuse.
        let result = tool
            .execute(&serde_json::json!({
                "path": "g.rs",
                "old_string": "fn f() {\n    a();\n}",
                "new_string": "fn f() {}"
            }))
            .await
            .unwrap();

        assert!(!result.success, "guard must reject: {}", result.output);
        assert!(
            result.output.contains("disproportionate"),
            "expected guard message, got: {}",
            result.output
        );
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("g.rs")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn should_reject_disproportionate_char_growth() {
        let dir = tempfile::tempdir().unwrap();
        let indent = " ".repeat(60);
        let original = format!("{indent}x();\n{indent}y();\n{indent}z();\n");
        std::fs::write(dir.path().join("c.rs"), &original).unwrap();

        let tool = EditFileTool::new(dir.path());
        // line_trimmed would match, but the span is 194 bytes for a
        // 14-byte old_string (> 4x) — the guard must refuse.
        let result = tool
            .execute(&serde_json::json!({
                "path": "c.rs",
                "old_string": "x();\ny();\nz();",
                "new_string": "w();"
            }))
            .await
            .unwrap();

        assert!(!result.success, "guard must reject: {}", result.output);
        assert!(result.output.contains("disproportionate"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("c.rs")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn should_fail_with_count_when_fuzzy_match_is_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let original = "fn a() {\n    if ready {\n        launch();\n    }\n}\nfn b() {\n  if ready {\n    launch();\n  }\n}\n";
        std::fs::write(dir.path().join("amb.rs"), original).unwrap();

        let tool = EditFileTool::new(dir.path());
        // No exact occurrence, but the trimmed block exists at two
        // different indentation levels — must fail with the count, not
        // silently pick one or fall through to a fuzzier stage.
        let result = tool
            .execute(&serde_json::json!({
                "path": "amb.rs",
                "old_string": "if ready {\nlaunch();\n}",
                "new_string": "abort();"
            }))
            .await
            .unwrap();

        assert!(!result.success, "{}", result.output);
        assert!(
            result.output.contains("2 occurrences"),
            "expected the ambiguity count, got: {}",
            result.output
        );
        assert!(result.output.contains("line_trimmed"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("amb.rs")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn should_reject_empty_old_string() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "content").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "f.txt",
                "old_string": "",
                "new_string": "x"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("must not be empty"));
    }

    #[tokio::test]
    async fn should_keep_exact_success_output_stable() {
        // Exact matches keep the historical wording — no replacer chatter.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.txt"), "hello world\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "s.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output, "Successfully edited s.txt");
    }

    #[tokio::test]
    async fn should_splice_fuzzy_match_at_located_span_not_first_substring() {
        // The fuzzy-matched span's exact text ("    a();\n    b();") ALSO
        // occurs earlier in the file, but mid-line (inside a string-ish
        // context), where it is not a valid line window. A string replacen
        // of the matched text would corrupt the earlier occurrence; the
        // byte-range splice must edit the located window only.
        let dir = tempfile::tempdir().unwrap();
        let original = "code(    a();\n    b();x)\nfn late() {\n    a();\n    b();\n}\n";
        std::fs::write(dir.path().join("span.rs"), original).unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "span.rs",
                // Flush-left needle: line_trimmed matches only the window
                // inside fn late().
                "old_string": "a();\nb();",
                "new_string": "    c();"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        let content = std::fs::read_to_string(dir.path().join("span.rs")).unwrap();
        assert_eq!(
            content,
            "code(    a();\n    b();x)\nfn late() {\n    c();\n}\n"
        );
    }

    // -----------------------------------------------------------------------
    // #1767: industry-convention parameter aliases.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_accept_camel_case_aliases_for_edit_input() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alias.txt"), "hello world\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "filePath": "alias.txt",
                "oldString": "hello",
                "newString": "goodbye"
            }))
            .await
            .unwrap();

        assert!(result.success, "aliases must work: {}", result.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("alias.txt")).unwrap(),
            "goodbye world\n"
        );
    }

    #[test]
    fn schema_advertises_canonical_names_only() {
        let tool = EditFileTool::new("/tmp");
        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("path"));
        assert!(props.contains_key("old_string"));
        assert!(props.contains_key("new_string"));
        assert!(!props.contains_key("filePath"));
        assert!(!props.contains_key("oldString"));
        assert!(!props.contains_key("newString"));
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
