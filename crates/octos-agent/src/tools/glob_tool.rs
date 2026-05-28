//! Glob tool for finding files by pattern.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use globset::{GlobBuilder, GlobSetBuilder};
use ignore::WalkBuilder;
use octos_core::{PathClassification, SessionScope};
use serde::Deserialize;

use super::{Tool, ToolContext, ToolResult};
use crate::policy::FilesystemScope;

/// Tool for finding files matching a glob pattern.
pub struct GlobTool {
    /// Base directory for searches.
    base_dir: PathBuf,
    /// Effective filesystem scope.
    filesystem_scope: FilesystemScope,
}

impl GlobTool {
    /// Create a new glob tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
        }
    }

    /// Set the effective filesystem scope.
    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
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
        let input: GlobInput =
            serde_json::from_value(args.clone()).wrap_err("invalid glob tool input")?;

        let pattern = input.pattern.clone();
        let limit = input.limit;
        let filesystem_scope = self.filesystem_scope;

        // Reject `..` and absolute paths uniformly in scoped mode too.
        //
        // Round-2 codex follow-up: the scoped branch previously relaxed
        // `..` rejection on the theory that canonicalize+classify would
        // catch any escape at output time. That's true for the output,
        // but the underlying `glob::glob` walker can still TRAVERSE
        // out-of-scope directories during recursion (it follows
        // symlinks). The structural fix is to replace `glob::glob` with
        // a scoped `ignore::WalkBuilder` walker (see `run_glob_scoped`
        // below); the `..` rejection here is now defense-in-depth and
        // gives the LLM a clear error message rather than silently
        // returning zero matches.
        if !filesystem_scope.is_host() && pattern.contains("..") {
            return Ok(ToolResult {
                output: "Absolute paths and '..' are not allowed in glob patterns".to_string(),
                success: false,
                ..Default::default()
            });
        }
        if ctx.session_scope.is_none() && !filesystem_scope.is_host() && pattern.starts_with('/') {
            return Ok(ToolResult {
                output: "Absolute paths and '..' are not allowed in glob patterns".to_string(),
                success: false,
                ..Default::default()
            });
        }

        let scope = ctx.session_scope.clone();
        let base_dir = self.base_dir.clone();
        let pattern_clone = pattern.clone();

        // Run glob in blocking task. Scoped vs legacy branches diverge
        // entirely so each implementation is self-contained.
        let result = tokio::task::spawn_blocking(move || match scope {
            Some(scope) => run_glob_scoped(&scope, pattern_clone, limit),
            None => run_glob_legacy(base_dir, filesystem_scope, pattern_clone, limit),
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

/// Split a glob pattern into the longest non-glob prefix and the
/// remaining pattern. The prefix is the leading portion that contains
/// no glob metacharacters (`*`, `?`, `[`); we walk back to the last `/`
/// before the first metachar so the prefix names a real directory we
/// can root a `WalkBuilder` at.
///
/// Returns `(prefix, remainder)` where `prefix` may be empty (no `/`
/// before the first metachar) and `remainder` is the rest of the
/// pattern after the prefix (which may itself contain literal path
/// components followed by metacharacters).
///
/// Examples:
/// - `"**/*.rs"`                   -> (`""`, `"**/*.rs"`)
/// - `"src/**/*.rs"`               -> (`"src"`, `"**/*.rs"`)
/// - `"src/lib.rs"`                -> (`"src/lib.rs"`, `""`)
/// - `"/abs/path/styles/*.toml"`   -> (`"/abs/path/styles"`, `"*.toml"`)
fn split_glob_prefix(pattern: &str) -> (String, String) {
    let bytes = pattern.as_bytes();
    let mut first_meta: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(b, b'*' | b'?' | b'[') {
            first_meta = Some(i);
            break;
        }
    }
    match first_meta {
        None => (pattern.to_string(), String::new()),
        Some(idx) => {
            // Walk back to the last `/` before idx; the prefix ends at
            // that separator (exclusive of the leading content up to
            // and including the slash).
            let prefix_end = pattern[..idx].rfind('/').map(|p| p + 1).unwrap_or(0);
            let prefix = pattern[..prefix_end].trim_end_matches('/').to_string();
            let remainder = pattern[prefix_end..].to_string();
            (prefix, remainder)
        }
    }
}

/// Scoped walker for `SessionScope`-aware glob execution.
///
/// Design (codex round-2 BLOCKER fix):
/// 1. Compute the longest non-glob prefix of the pattern.
/// 2. Anchor the walker root: relative patterns root at
///    `scope.workspace().join(prefix)`; absolute patterns use the
///    prefix verbatim (with `/` as a degenerate root that classifies
///    `OutOfScope`).
/// 3. Canonicalize + classify the walker root. If it lands
///    `OutOfScope`, refuse before walking.
/// 4. Build a `globset::GlobSet` from the **remaining** pattern. The
///    walker enumerates real on-disk entries; we match the entry's
///    path-relative-to-root against the globset.
/// 5. Walk via `ignore::WalkBuilder::follow_links(false)` so symlinks
///    are NOT traversed during descent. The walker still surfaces the
///    symlink ENTRY itself; canonicalize+classify drops it if the
///    target escapes scope.
fn run_glob_scoped(scope: &SessionScope, pattern: String, limit: usize) -> Result<Vec<String>> {
    // Step 1 + 2: compute prefix and anchor walker root.
    let pattern_path = PathBuf::from(&pattern);
    let (prefix, remainder) = split_glob_prefix(&pattern);
    let (root, glob_pattern): (PathBuf, String) = if pattern_path.is_absolute() {
        // Absolute pattern. Prefix is the absolute prefix; remainder is
        // matched relative to that prefix.
        let prefix_path = if prefix.is_empty() {
            // Degenerate case: pattern starts with `/*` etc. Use `/`
            // as the walker root; classification will refuse it.
            PathBuf::from("/")
        } else {
            PathBuf::from(&prefix)
        };
        (prefix_path, remainder)
    } else {
        // Relative pattern. Resolve `<workspace>/<prefix>` as the root;
        // the remainder is the globset pattern matched against
        // entry-relative-to-root.
        let root = if prefix.is_empty() {
            scope.workspace().to_path_buf()
        } else {
            scope.workspace().join(&prefix)
        };
        (root, remainder)
    };

    // Step 3: canonicalize + classify the walker root before descent.
    // Refuses any pattern whose non-glob prefix already escapes scope —
    // we never start a walk in out-of-scope territory.
    if matches!(
        scope.classify_canonical_path(&root),
        PathClassification::OutOfScope
    ) {
        // Refusing here is semantically the same as "no matches" from
        // the LLM's perspective; the canonical-classify guard would
        // also drop every entry anyway. We choose the cheap exit.
        return Ok(Vec::new());
    }

    // Step 4: compile globset from the remainder pattern. When
    // `glob_pattern` is empty, the pattern was a pure literal path; we
    // treat the root itself as a single match if it exists and is a
    // file.
    let glob_set = if glob_pattern.is_empty() {
        None
    } else {
        let glob = GlobBuilder::new(&glob_pattern)
            .literal_separator(true)
            .build()
            .wrap_err_with(|| format!("invalid glob pattern: {}", glob_pattern))?;
        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        Some(builder.build().wrap_err("globset build failed")?)
    };

    let mut files: Vec<String> = Vec::new();

    // Literal-only pattern fast path: no walking needed.
    if glob_set.is_none() {
        if root.is_file()
            && !matches!(
                scope.classify_canonical_path(&root),
                PathClassification::OutOfScope
            )
        {
            let display = root
                .strip_prefix(scope.workspace())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| root.clone());
            files.push(display.display().to_string());
        }
        return Ok(files);
    }
    let glob_set = glob_set.expect("checked above");

    // Step 5: walk with follow_links(false) so symlinks aren't traversed.
    let walker = WalkBuilder::new(&root)
        .follow_links(false)
        .hidden(false)
        .git_ignore(false)
        .build();

    for entry in walker {
        if files.len() >= limit {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "glob walker entry error");
                continue;
            }
        };
        let path = entry.path();

        // Skip the walker root itself (matches the prior `glob::glob`
        // behaviour, which never returned the literal anchor as a
        // result).
        if path == root {
            continue;
        }

        // Skip directories — glob matches files only.
        if path.is_dir() {
            continue;
        }

        // Per-entry canonicalize+classify. Closes the symlink-leaf
        // hole: a symlink under `root` pointing at `/etc/passwd` would
        // surface as `<root>/escape`; canonicalize resolves to
        // `/etc/passwd`, which classifies `OutOfScope`. The PRIMARY
        // containment guarantee comes from `follow_links(false)`
        // pruning subtree descent; this is the defence-in-depth check
        // for individual entries.
        if matches!(
            scope.classify_canonical_path(path),
            PathClassification::OutOfScope
        ) {
            continue;
        }

        // Match the entry against the globset using the
        // entry-relative-to-root path. Both `**/*.rs` and `*.rs`
        // patterns should match files at any depth (when `**` is
        // present) or only at the root depth (otherwise).
        let rel = match path.strip_prefix(&root) {
            Ok(p) => p,
            Err(_) => continue,
        };

        if !glob_set.is_match(rel) {
            continue;
        }

        // Display path: relative to `scope.workspace()` when possible.
        let display_path = path
            .strip_prefix(scope.workspace())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.to_path_buf());
        files.push(display_path.display().to_string());
    }

    Ok(files)
}

/// Legacy `base_dir + FilesystemScope` glob execution (pre-PR-B).
///
/// Kept unchanged for callers without a `SessionScope`: `octos chat`
/// uses this path. The `glob::glob` recursion model is acceptable here
/// because the only containment guarantee these callers ever had was
/// the lexical `..` / absolute rejection at the input boundary plus
/// the `base_dir` anchor — that's still in force.
fn run_glob_legacy(
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    pattern: String,
    limit: usize,
) -> Result<Vec<String>> {
    let pattern_path = PathBuf::from(&pattern);
    let full_pattern = if filesystem_scope.is_host() && pattern_path.is_absolute() {
        pattern.clone()
    } else {
        format!("{}/{}", base_dir.display(), pattern)
    };

    let mut files: Vec<String> = Vec::new();

    let entries = match glob::glob(&full_pattern) {
        Ok(p) => p,
        Err(e) => return Err(eyre::eyre!("invalid glob pattern: {}", e)),
    };

    for entry in entries {
        if files.len() >= limit {
            break;
        }
        let path = match entry {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "glob entry error");
                continue;
            }
        };

        let display_path = path
            .strip_prefix(&base_dir)
            .map(|p| p.to_path_buf())
            .unwrap_or(path);
        files.push(display_path.display().to_string());
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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

    // ------------------------------------------------------------------
    // PR-B (round-1): SessionScope integration tests for GlobTool.
    // ------------------------------------------------------------------

    fn ctx_with_scope(scope: SessionScope) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "glob-with-scope".to_string();
        ctx.session_scope = Some(Arc::new(scope));
        ctx
    }

    #[tokio::test]
    async fn glob_into_skill_dir_returns_matches() {
        // Absolute pattern inside a registered skill_dir is accepted
        // when a SessionScope with `skill_read_zones` is wired.
        let workspace = tempfile::tempdir().unwrap();
        let skill = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(skill.path().join("styles")).unwrap();
        std::fs::write(skill.path().join("styles/light.toml"), "k=1").unwrap();
        std::fs::write(skill.path().join("styles/dark.toml"), "k=2").unwrap();
        std::fs::write(skill.path().join("styles/note.md"), "x").unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![])
            .unwrap()
            .with_skill_read_zones(vec![skill.path().to_path_buf()])
            .unwrap();

        let tool = GlobTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        // Absolute glob inside the registered skill_dir.
        let pattern = format!("{}/styles/*.toml", skill.path().display());
        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"pattern": pattern}))
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        assert!(
            result.output.contains("2 file(s)"),
            "expected two matches, got: {}",
            result.output
        );
        assert!(result.output.contains("light.toml"));
        assert!(result.output.contains("dark.toml"));
        assert!(!result.output.contains("note.md"));
    }

    #[tokio::test]
    async fn glob_traversal_pattern_drops_matches_outside_zones() {
        // Two cases the scoped walker must refuse:
        // (a) an absolute glob to a non-zone path — the walker root
        //     classifies OutOfScope, so we return zero matches without
        //     walking.
        // (b) a relative `..`-traversal pattern — the `..` rejection
        //     fires at input time. Codex round-2 MINOR: the prior test
        //     only covered (a); this version covers both.
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("passwd"), "root:x:0:0").unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
        let tool = GlobTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        // (a) Absolute pattern outside every zone.
        let pattern_a = format!("{}/*", outside.path().display());
        let result_a = tool
            .execute_with_context(&ctx, &serde_json::json!({"pattern": pattern_a}))
            .await
            .unwrap();
        assert!(result_a.success);
        assert!(
            result_a.output.contains("No files found"),
            "expected zero matches (root OutOfScope), got: {}",
            result_a.output
        );

        // (b) Relative `..` traversal pattern. Per codex round-2 the
        // scoped branch MUST refuse this — defence-in-depth alongside
        // the scoped walker, so the LLM gets a clear error rather than
        // silently no matches.
        let result_b = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"pattern": "**/../../../etc/passwd"}),
            )
            .await
            .unwrap();
        assert!(!result_b.success);
        assert!(
            result_b.output.contains("not allowed"),
            "expected `..` rejection, got: {}",
            result_b.output
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_drops_symlink_target_outside_scope() {
        // A symlink inside the workspace pointing at /etc would
        // otherwise let `<workspace>/link/passwd` masquerade as
        // workspace-resident. The scoped walker uses follow_links(false)
        // so the walker NEVER traverses into the symlink target; the
        // per-entry canonical-classify filter is the defence-in-depth
        // safety net.
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("passwd"), "root:x").unwrap();
        symlink(outside.path(), workspace.path().join("escape")).unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
        let tool = GlobTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"pattern": "escape/*"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            !result.output.contains("passwd"),
            "match traversing a symlink that leaves the workspace must be dropped, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn glob_falls_back_to_legacy_when_no_scope() {
        // No scope wired => pre-PR-B base_dir-anchored behaviour.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("legacy.rs"), "").unwrap();

        let tool = GlobTool::new(dir.path());
        let ctx = ToolContext::zero();
        assert!(ctx.session_scope.is_none());

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"pattern": "*.rs"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("legacy.rs"));
    }

    // ------------------------------------------------------------------
    // PR-B (round-2): scoped walker no-traversal proof.
    // ------------------------------------------------------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn scoped_walker_does_not_descend_into_out_of_scope_symlink() {
        // BLOCKER fix: the prior `glob::glob` walker could traverse
        // INTO a symlink target during recursion (it follows symlinks
        // by default). With `WalkBuilder::follow_links(false)` the
        // walker MUST NOT visit any entry inside the symlink's target.
        //
        // Construction: <workspace>/escape -> /tmp/<sensitive_dir>/
        // with `sensitive_dir/secret.txt` inside. A `**/*` glob rooted
        // at the workspace must NOT surface `secret.txt`.
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let sensitive = tempfile::tempdir().unwrap();
        // Write a uniquely-named sentinel inside the symlink target so
        // we can assert by name (not just by extension) below.
        std::fs::write(sensitive.path().join("escape_target_sentinel.txt"), "leak").unwrap();
        std::fs::write(sensitive.path().join("escape_target_sentinel.toml"), "k=v").unwrap();
        // Also stuff in some innocuous workspace content so the match
        // count proves the walker did run.
        std::fs::write(workspace.path().join("legit.txt"), "ok").unwrap();
        symlink(sensitive.path(), workspace.path().join("escape")).unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
        let tool = GlobTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"pattern": "**/*"}))
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        // The walker is allowed to surface the workspace-resident
        // file. It must NOT surface anything from the symlink target.
        assert!(
            result.output.contains("legit.txt"),
            "walker must visit workspace entries, got: {}",
            result.output
        );
        assert!(
            !result.output.contains("escape_target_sentinel"),
            "walker must NOT descend into the symlink target (entries from \
             /tmp/sensitive_dir present in output), got: {}",
            result.output
        );
    }

    // ------------------------------------------------------------------
    // PR-B (round-2): split_glob_prefix unit tests.
    // ------------------------------------------------------------------

    #[test]
    fn split_glob_prefix_no_metachars_is_pure_literal() {
        let (prefix, remainder) = split_glob_prefix("src/lib.rs");
        assert_eq!(prefix, "src/lib.rs");
        assert_eq!(remainder, "");
    }

    #[test]
    fn split_glob_prefix_leading_metachar_has_empty_prefix() {
        let (prefix, remainder) = split_glob_prefix("**/*.rs");
        assert_eq!(prefix, "");
        assert_eq!(remainder, "**/*.rs");
    }

    #[test]
    fn split_glob_prefix_literal_dir_then_metachar() {
        let (prefix, remainder) = split_glob_prefix("src/**/*.rs");
        assert_eq!(prefix, "src");
        assert_eq!(remainder, "**/*.rs");
    }

    #[test]
    fn split_glob_prefix_absolute_path() {
        let (prefix, remainder) = split_glob_prefix("/abs/path/styles/*.toml");
        assert_eq!(prefix, "/abs/path/styles");
        assert_eq!(remainder, "*.toml");
    }

    #[test]
    fn split_glob_prefix_metachar_mid_component() {
        // Pattern: `src/lib*.rs` — the first metachar is mid-component;
        // the non-glob prefix should walk back to the prior `/`, i.e.
        // `src`, with the rest as the globset pattern.
        let (prefix, remainder) = split_glob_prefix("src/lib*.rs");
        assert_eq!(prefix, "src");
        assert_eq!(remainder, "lib*.rs");
    }
}
