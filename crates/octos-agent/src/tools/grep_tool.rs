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
/// `display` is the text as it would be emitted (the plain match site trims
/// it first); `raw_line` is the physical line it came from. The cap fires on
/// `display` and cuts by char index (never inside a multi-byte UTF-8 char);
/// the suffix names the RAW line's char count, so the same physical line
/// reports the same `N chars total` at every emission site regardless of
/// site-local trimming. Re-running the search cannot reveal more of the
/// line; `read_file` can.
fn cap_match_line<'a>(display: &'a str, raw_line: &str) -> std::borrow::Cow<'a, str> {
    match display.char_indices().nth(GREP_MAX_LINE_LENGTH) {
        None => std::borrow::Cow::Borrowed(display),
        Some((cut, _)) => {
            let total_chars = raw_line.chars().count();
            std::borrow::Cow::Owned(format!(
                "{}\u{2026} [line truncated, {total_chars} chars total]",
                &display[..cut]
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
    // The exemption is for a search root that is itself outside the scope
    // (the resolved upload file). A search root inside the scope never
    // exempts an out-of-scope entry below it: a request context searching
    // its peer's folder (UPCR-2026-034 `read_parent`) must not reach another
    // context's folder under it.
    let search_root_out_of_scope = scope.as_ref().is_some_and(|scope| {
        matches!(
            scope.classify_canonical_path(&search_root),
            PathClassification::OutOfScope
        )
    });

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
            ) && !(search_root_out_of_scope
                && octos_core::canonicalize_lossy(path).starts_with(&canonical_search_root))
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
                            cap_match_line(ctx_line, ctx_line)
                        ));
                    }
                    matches.push(ctx_output);
                } else {
                    matches.push(format!(
                        "{}:{}: {}",
                        rel_path,
                        line_num + 1,
                        cap_match_line(line.trim(), line)
                    ));
                }
            }
        }
    }

    Ok((matches, match_count))
}
