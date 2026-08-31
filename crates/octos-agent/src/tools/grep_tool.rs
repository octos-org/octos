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
use crate::policy::FilesystemScope;

/// Tool for searching file contents with regex.
pub struct GrepTool {
    /// Base directory for searches.
    base_dir: PathBuf,
    /// Effective filesystem reach for the legacy (no-`SessionScope`) path. When
    /// `Host`, an explicit `path` arg may point OUTSIDE `base_dir` (mirrors
    /// read_file/glob/list_dir); `Workspace` (default) confines to `base_dir`.
    filesystem_scope: FilesystemScope,
}

impl GrepTool {
    /// Create a new grep tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
        }
    }

    /// Set the effective filesystem scope. `Host` lets an explicit `path` arg
    /// escape `base_dir`, consistent with the other cwd-bound read tools (so a
    /// worker granted host FS can grep outside its cwd, not just read/glob).
    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }
}

#[derive(Debug, Deserialize)]
// #1770: unknown keys are usually a typo of a real parameter; rejecting
// them (with a did-you-mean via `args::parse_tool_args`) lets the model
// self-correct instead of silently dropping its intent.
#[serde(deny_unknown_fields)]
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

/// Max chars kept from a single emitted match/context line — pi's
/// `GREP_MAX_LINE_LENGTH` (`packages/coding-agent/src/core/tools/truncate.ts`).
///
/// Without this, one minified-JS line consumes the whole grep output budget
/// (`octos_core::tool_output_limit("grep")`) blind: the backstop head/tail cut
/// then elides every later match while the model sees mostly one giant line.
const GREP_MAX_LINE_LENGTH: usize = 500;

/// Cap one emitted line at [`GREP_MAX_LINE_LENGTH`] characters.
///
/// Cuts by char index (never inside a multi-byte UTF-8 char) and appends a
/// suffix naming the original length, so the model knows what it lost —
/// re-running the search cannot reveal more; `read_file` can.
fn cap_match_line(line: &str) -> std::borrow::Cow<'_, str> {
    match line.char_indices().nth(GREP_MAX_LINE_LENGTH) {
        None => std::borrow::Cow::Borrowed(line),
        Some((cut, _)) => {
            let total_chars = GREP_MAX_LINE_LENGTH + line[cut..].chars().count();
            std::borrow::Cow::Owned(format!(
                "{}\u{2026} [line truncated, {total_chars} chars total]",
                &line[..cut]
            ))
        }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        static DESCRIPTION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            format!(
                "Search file contents using regex. Respects .gitignore. Use file_pattern to \
                 filter which files to search (e.g., '*.rs'). Use path to scope the search to a \
                 specific directory. Output is truncated beyond {} bytes (middle elided) and \
                 each matched line beyond {GREP_MAX_LINE_LENGTH} chars, so prefer narrow \
                 patterns and path/file_pattern filters over broad sweeps.",
                octos_core::tool_output_limit("grep")
            )
        });
        &DESCRIPTION
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

    /// `grep` does not paginate, so the recovery is to NARROW, and the advice
    /// says which lever is still unused rather than offering a generic hint.
    fn truncation_recovery(
        &self,
        args: &serde_json::Value,
        omitted_bytes: usize,
    ) -> Option<String> {
        let mut levers = Vec::new();
        if args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            levers.push("scope it with path");
        }
        if args
            .get("file_pattern")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            levers.push("filter files with file_pattern (for example \"*.rs\")");
        }
        levers.push("tighten the pattern");
        levers.push("lower limit and re-run");
        Some(format!(
            "[{omitted_bytes} bytes omitted] Too many matches to return. Narrow the search: {}. \
             Matched lines are already capped at {GREP_MAX_LINE_LENGTH} chars — re-running \
             cannot reveal more of a long line; use read_file to see one in full.",
            levers.join(", ")
        ))
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
            super::args::parse_tool_args(self.name(), &self.input_schema(), args)?;

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
                // Honor the filesystem scope: `Host` lets an explicit path leave
                // base_dir (consistent with read_file/glob/list_dir under a host
                // FS grant); `Workspace` confines to base_dir.
                Some(p) => {
                    match super::resolve_path_with_scope(&self.base_dir, p, self.filesystem_scope) {
                        Ok(root) => root,
                        Err(_) => {
                            return Ok(ToolResult {
                                output: format!("Path outside working directory: {p}"),
                                success: false,
                                ..Default::default()
                            });
                        }
                    }
                }
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
                format!(" (limited to {limit})")
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
    // Canonical form of the resolved search root. Used by the per-entry scope
    // guard to exempt the legitimately-rooted upload file (see below).
    let canonical_search_root = octos_core::canonicalize_lossy(&search_root);

    // Compile regex.
    let regex_pattern = if ignore_case {
        format!("(?i){pattern_str}")
    } else {
        pattern_str.clone()
    };
    let regex = RegexBuilder::new(&regex_pattern)
        .size_limit(1 << 20) // 1 MB compiled regex limit (prevents ReDoS)
        .build()
        .wrap_err_with(|| format!("invalid regex: {pattern_str}"))?;

    // Compile the file-name glob once and fail loudly on an invalid pattern.
    // Previously an uncompilable glob was silently ignored, causing grep to
    // search *every* file instead of the intended subset.
    let file_glob = match file_pattern {
        Some(ref fp) => Some(
            glob::Pattern::new(fp).wrap_err_with(|| format!("invalid file_pattern glob: {fp}"))?,
        ),
        None => None,
    };

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
        //
        // EXCEPT the file the search was explicitly rooted at: when `path` was
        // an upload handle (`up/...`), `search_root` is the resolved
        // upload-tmpdir file, which classifies OutOfScope because uploads live
        // outside any SessionScope (see `resolve_for_scope`). Without an
        // exemption grep would resolve the handle then silently report "No
        // matches". We exempt ONLY entries whose canonical path is contained in
        // the canonical search root — i.e. the upload file the caller actually
        // asked to search.
        //
        // SECURITY: keying the exemption on containment in `search_root` (not
        // on "is under the global upload tmpdir") is what makes it leak-proof.
        // A symlink whose target is a SIBLING upload, `/etc/passwd`, or any
        // other tenant's file canonicalises OUTSIDE `search_root`, so it is not
        // exempt and stays dropped — even when the workspace itself is nested
        // under the upload tmpdir. For a normal workspace walk `search_root` is
        // the in-scope workspace, so its files never classify OutOfScope and
        // this exemption is inert.
        // `&&` short-circuits, so the second `canonicalize_lossy` runs ONLY for
        // the rare OutOfScope entry (almost never during a normal workspace
        // walk) — `classify_canonical_path` already canonicalised once, so we
        // don't pay a second stat per in-scope file.
        if let Some(scope) = scope.as_ref() {
            if matches!(
                scope.classify_canonical_path(path),
                PathClassification::OutOfScope
            ) && !octos_core::canonicalize_lossy(path).starts_with(&canonical_search_root)
            {
                continue;
            }
        }

        // Apply file pattern filter.
        if let Some(ref p) = file_glob {
            let file_name = path.file_name().unwrap_or_default().to_string_lossy();
            if !p.matches(&file_name) {
                continue;
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

                    let mut ctx_output = format!("{rel_path}:\n");
                    for (i, ctx_line) in lines[start..end].iter().enumerate() {
                        let actual_line = start + i;
                        let marker = if actual_line == line_num { ">" } else { " " };
                        ctx_output.push_str(&format!(
                            "{} {:4}│ {}\n",
                            marker,
                            actual_line + 1,
                            cap_match_line(ctx_line)
                        ));
                    }
                    matches.push(ctx_output);
                } else {
                    matches.push(format!(
                        "{}:{}: {}",
                        rel_path,
                        line_num + 1,
                        cap_match_line(line.trim())
                    ));
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
    async fn test_grep_invalid_file_pattern_errors() {
        // Regression: an uncompilable file_pattern glob used to be silently
        // ignored, searching every file. It must now error loudly.
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "hello", "file_pattern": "[bad"}))
            .await;

        let err = match result {
            Ok(_) => panic!("expected an error for an invalid file_pattern glob"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("invalid file_pattern glob"), "got: {err}");
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
    async fn grep_host_scope_reads_outside_base_dir() {
        // PR A fix #3: with FilesystemScope::Host (a host FS grant) an explicit
        // `path` outside base_dir is searched — consistent with read/glob/list.
        // The default (Workspace) still refuses it.
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("data.txt"), "NEEDLE lives here\n").unwrap();

        // Workspace scope (default): the out-of-cwd path is refused.
        let confined = GrepTool::new(base.path());
        let refused = confined
            .execute(&serde_json::json!({
                "pattern": "NEEDLE",
                "path": outside.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        assert!(
            !refused.success,
            "workspace scope must refuse an outside path"
        );
        assert!(refused.output.contains("outside working directory"));

        // Host scope: the same path is searched and the match is found.
        let host = GrepTool::new(base.path()).with_filesystem_scope(FilesystemScope::Host);
        let found = host
            .execute(&serde_json::json!({
                "pattern": "NEEDLE",
                "path": outside.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        assert!(
            found.success,
            "host scope must search the path, got: {}",
            found.output
        );
        assert!(found.output.contains("NEEDLE"), "got: {}", found.output);
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

    #[tokio::test]
    async fn grep_searches_resolved_upload_handle_contents() {
        // codex #1367 P2: an uploaded file lives in the upload tmpdir, which
        // is OUTSIDE the SessionScope. Once `resolve_for_scope` decodes the
        // `up/...` handle to the real upload path, grep's per-entry OutOfScope
        // filter must NOT drop it — otherwise grep resolves the handle then
        // silently returns "No matches" for a file the user uploaded.
        let upload_root = octos_bus::file_handle::temp_upload_root();
        std::fs::create_dir_all(&upload_root).unwrap();
        let uploaded = upload_root.join(format!("g-{}-insight.md", std::process::id()));
        std::fs::write(
            &uploaded,
            "# strategy insight\nNEEDLE marks the spot in the uploaded report\n",
        )
        .unwrap();
        let handle =
            octos_bus::file_handle::encode_tmp_upload_handle(&uploaded, Some("insight.md"))
                .expect("encode upload handle");

        // Workspace is unrelated to the upload tmpdir.
        let workspace = tempfile::tempdir().unwrap();
        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();

        let tool = GrepTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "pattern": "NEEDLE",
                    "path": handle,
                }),
            )
            .await
            .unwrap();

        let _ = std::fs::remove_file(&uploaded);

        // Assert on REAL match output, not the echoed pattern: the no-match
        // message is `No matches found for pattern: NEEDLE` (which contains the
        // pattern), so checking for "NEEDLE" alone would pass even when the
        // file was dropped. The match line carries the surrounding text, which
        // the no-match message never does.
        assert!(result.success, "expected success, got: {}", result.output);
        assert!(
            result.output.starts_with("Found ")
                && result
                    .output
                    .contains("marks the spot in the uploaded report"),
            "grep must surface the uploaded file's matched line, got: {}",
            result.output
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_does_not_follow_workspace_symlink_into_upload_root() {
        // codex #1367 round-2 P1: the upload-root exemption must apply ONLY
        // when the search is explicitly rooted at an upload handle. A workspace
        // symlink whose target sits under the GLOBAL upload tmpdir must still be
        // dropped — otherwise a scoped session could read arbitrary uploads
        // (other tenants' files) by planting a symlink in its own workspace.
        let upload_root = octos_bus::file_handle::temp_upload_root();
        std::fs::create_dir_all(&upload_root).unwrap();
        let secret = upload_root.join(format!("s-{}-secret.md", std::process::id()));
        std::fs::write(
            &secret,
            "TENANT_SECRET must never leak via a workspace symlink\n",
        )
        .unwrap();

        let workspace = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&secret, workspace.path().join("leak.md")).unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
        let tool = GrepTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        // Search the WORKSPACE (not the upload handle): rooted_in_upload=false,
        // so the exemption must not fire and the symlink target stays dropped.
        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({ "pattern": "TENANT_SECRET" }))
            .await
            .unwrap();

        let _ = std::fs::remove_file(&secret);

        assert!(
            !result
                .output
                .contains("must never leak via a workspace symlink"),
            "scoped grep must NOT follow a workspace symlink into the upload tmpdir, got: {}",
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

    /// grep cannot paginate, so its recovery is narrowing — and the advice
    /// should name the levers still UNUSED rather than listing all of them.
    #[test]
    fn should_offer_the_unused_narrowing_levers_when_matches_overflow() {
        let tool = GrepTool::new(std::path::Path::new("."));
        let advice = tool
            .truncation_recovery(&serde_json::json!({ "pattern": "fn " }), 9_000)
            .expect("grep always has narrowing available");
        assert!(advice.contains("9000 bytes omitted"), "{advice}");
        assert!(
            advice.contains("path"),
            "unused path filter should be offered: {advice}"
        );
        assert!(
            advice.contains("file_pattern"),
            "unused file_pattern filter should be offered: {advice}"
        );

        let already_scoped = tool
            .truncation_recovery(
                &serde_json::json!({ "pattern": "fn ", "path": "crates", "file_pattern": "*.rs" }),
                9_000,
            )
            .expect("still recoverable by tightening the pattern");
        assert!(
            !already_scoped.contains("scope it with path"),
            "a lever already in use must not be suggested again: {already_scoped}"
        );
    }

    // ── per-match-line cap (pi GREP_MAX_LINE_LENGTH port) ────────────────

    /// One minified-JS line must not blow the whole 30KB grep budget: each
    /// emitted match line is capped at 500 chars with a suffix naming the
    /// original length. Driven through the REAL execute path.
    #[tokio::test]
    async fn should_cap_match_line_at_500_chars_when_line_is_longer() {
        let dir = tempfile::tempdir().unwrap();
        let long_line = format!("needle{}", "a".repeat(600)); // 606 chars
        std::fs::write(
            dir.path().join("minified.js"),
            format!("{long_line}\nneedle short\n"),
        )
        .unwrap();

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({ "pattern": "needle" }))
            .await
            .unwrap();
        assert!(result.success, "{}", result.output);
        // Mutation guard: deleting the cap re-emits the full 600-char run.
        assert!(
            !result.output.contains(&"a".repeat(501)),
            "match line emitted uncapped"
        );
        assert!(
            result
                .output
                .contains(&format!("… [line truncated, {} chars total]", 606)),
            "cap suffix must name the original length: {}",
            result.output
        );
        // Exactly 500 chars of payload survive: "needle" + 494 'a's.
        assert!(
            result
                .output
                .contains(&format!("needle{}…", "a".repeat(494))),
            "cap must keep exactly 500 chars: {}",
            result.output
        );
        // A short sibling match is untouched.
        assert!(result.output.contains("needle short"));
    }

    #[tokio::test]
    async fn should_not_cap_match_line_when_exactly_500_chars() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("needle{}", "b".repeat(494)); // exactly 500 chars
        std::fs::write(dir.path().join("edge.txt"), format!("{line}\n")).unwrap();

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({ "pattern": "needle" }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            result.output.contains(&line),
            "an exactly-at-limit line must pass through untouched: {}",
            result.output
        );
        assert!(
            !result.output.contains("[line truncated"),
            "no cap suffix at the boundary: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn should_cap_multibyte_match_line_at_char_boundary_when_over_limit() {
        let dir = tempfile::tempdir().unwrap();
        // 6 + 600 = 606 chars, mostly 3-byte CJK: a byte-indexed cut would
        // split a char and panic (or emit invalid UTF-8).
        let line = format!("needle{}", "\u{754C}".repeat(600));
        std::fs::write(dir.path().join("cjk.txt"), format!("{line}\n")).unwrap();

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({ "pattern": "needle" }))
            .await
            .unwrap();
        assert!(result.success, "{}", result.output);
        assert!(
            result
                .output
                .contains("… [line truncated, 606 chars total]"),
            "{}",
            result.output
        );
        assert!(
            result
                .output
                .contains(&format!("needle{}…", "\u{754C}".repeat(494))),
            "the cap must count CHARS, not bytes: {}",
            result.output
        );
        assert!(!result.output.contains(&"\u{754C}".repeat(495)));
    }

    #[tokio::test]
    async fn should_cap_context_lines_when_context_requested() {
        let dir = tempfile::tempdir().unwrap();
        let long_neighbor = "c".repeat(700);
        std::fs::write(
            dir.path().join("ctx.txt"),
            format!("{long_neighbor}\nneedle here\n"),
        )
        .unwrap();

        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({ "pattern": "needle", "context": 1 }))
            .await
            .unwrap();
        assert!(result.success, "{}", result.output);
        assert!(
            !result.output.contains(&"c".repeat(501)),
            "context lines must be capped too: one long neighbour blows the budget just the same"
        );
        assert!(
            result
                .output
                .contains("… [line truncated, 700 chars total]"),
            "{}",
            result.output
        );
    }

    /// With the per-line cap in place, a long line can no longer be recovered
    /// by re-running grep — the recovery advice must say so and point at
    /// read_file instead.
    #[test]
    fn should_mention_per_line_cap_in_recovery_when_matches_overflow() {
        let tool = GrepTool::new(std::path::Path::new("."));
        let advice = tool
            .truncation_recovery(&serde_json::json!({ "pattern": "x" }), 1_000)
            .expect("grep always has narrowing available");
        assert!(advice.contains("500 chars"), "{advice}");
        assert!(advice.contains("read_file"), "{advice}");
    }

    /// pi-style truncation contract: the model is warned about the output cap
    /// UP FRONT, in the tool description, using the real limits.
    #[test]
    fn should_state_truncation_contract_in_description_when_grep() {
        let tool = GrepTool::new(std::path::Path::new("."));
        let desc = tool.description();
        let limit = octos_core::tool_output_limit("grep");
        assert!(
            desc.contains(&limit.to_string()),
            "description must carry the real output cap ({limit}): {desc}"
        );
        assert!(
            desc.contains("500"),
            "description must carry the per-line cap: {desc}"
        );
    }
}
