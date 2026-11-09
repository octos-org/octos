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
// #1770: unknown keys are usually a typo of a real parameter; rejecting
// them (with a did-you-mean via `args::parse_tool_args`) lets the model
// self-correct instead of silently dropping its intent.
#[serde(deny_unknown_fields)]
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

/// Directory NAMES whose subtrees are pruned during glob DESCENT.
///
/// mini5 soak motivation: a broad glob from an unscoped cwd (the home dir)
/// recursed into `~/Library/...`, emitting thousands of per-entry WARN lines
/// and running until the 1800s tool timeout fired. These trees are either
/// system/OS storage (`Library`) or build/VCS noise every search tool
/// (ripgrep, fd, the `ignore` crate) skips by default. Pruning them at
/// descent keeps a broad scan fast without flooding logs.
///
/// Conservative invariant: this is a DESCENT filter only — it never affects a
/// pattern that explicitly anchors AT one of these directories (the walker
/// root is never pruned), and it never touches files at any other depth.
const NOISY_SKIP_DIRS: &[&str] = &[
    "Library",      // macOS user Library (huge, system-managed)
    ".git",         // VCS internals
    "node_modules", // JS dependency tree
    "target",       // Rust build output
];

/// Whether a directory with this name should be pruned during glob descent.
fn is_noisy_skip_dir(name: &str) -> bool {
    NOISY_SKIP_DIRS.contains(&name)
}

/// Returns `true` if `dir_entry` is a directory that should be pruned from
/// traversal: either it is a well-known noisy/system dir (and not the walker
/// root itself), so we never descend into it. Used as a `WalkBuilder`
/// `filter_entry` predicate (the closure returns `false` to prune).
fn should_descend(dir_entry: &ignore::DirEntry, root: &std::path::Path) -> bool {
    // Never prune the walker root — anchoring a pattern AT a noisy dir must
    // still enumerate its contents.
    if dir_entry.path() == root {
        return true;
    }
    // Only directories are descent candidates; files always pass.
    let is_dir = dir_entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
    if !is_dir {
        return true;
    }
    let prune = dir_entry
        .file_name()
        .to_str()
        .map(is_noisy_skip_dir)
        .unwrap_or(false);
    !prune
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
            super::args::parse_tool_args(self.name(), &self.input_schema(), args)?;

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

        // Interim mitigation (#1378, superseded by #1377): redirect upload-handle
        // / `up/` namespace patterns to read_file BEFORE globbing, so a decoded
        // handle is never returned as a workspace match (consistent with
        // read_file/list_dir which treat decoded `up/...` as uploads). A
        // non-handle `up/...` beside a real `up/` dir returns None and globs
        // normally; one with no real `up/` dir is redirected here rather than
        // surfacing the misleading empty "No files found".
        let ws_root = ctx
            .session_scope
            .as_ref()
            .map(|s| s.workspace().to_path_buf())
            .unwrap_or_else(|| self.base_dir.clone());
        if let Some(guidance) = super::upload_namespace_redirect(&pattern, &ws_root) {
            return Ok(ToolResult {
                output: guidance.to_string(),
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
            .wrap_err_with(|| format!("invalid glob pattern: {glob_pattern}"))?;
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
    // FIX 2: prune well-known noisy/system trees (`Library`, `.git`,
    // `node_modules`, `target`) during DESCENT via `filter_entry` so a broad
    // glob stays fast. The walker root is never pruned (anchoring AT such a
    // dir still enumerates it).
    let walk_root = root.clone();
    let walker = WalkBuilder::new(&root)
        .follow_links(false)
        .hidden(false)
        .git_ignore(false)
        .filter_entry(move |e| should_descend(e, &walk_root))
        .build();

    for entry in walker {
        if files.len() >= limit {
            // FIX 2: log ONCE on truncation (no silent unbounded scans).
            tracing::info!(limit, "glob result truncated at limit");
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // FIX 2: an unreadable/permission-denied subtree must NOT
                // emit a per-entry WARN (the mini5 log flood). Record at
                // debug and skip silently.
                tracing::debug!(error = %e, "glob walker entry skipped (unreadable)");
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
/// Used by callers without a `SessionScope` (`octos chat`, and the
/// unscoped `serve` factory path). The containment guarantee is the
/// lexical `..` / absolute rejection at the input boundary plus the
/// `base_dir` anchor — that's still in force.
///
/// FIX 2 (mini5 soak): this path previously used `glob::glob`, which (a)
/// recursed into `~/Library/...` when the cwd was an unscoped home dir,
/// emitting thousands of per-entry `glob entry error attempting to read`
/// WARN lines, and (b) ran until the 1800s tool timeout fired. It now
/// uses an `ignore::WalkBuilder` rooted at the pattern's non-glob prefix
/// with `filter_entry` pruning of well-known noisy/system trees
/// (`Library`, `.git`, `node_modules`, `target`) and debug-level (never
/// WARN) handling of unreadable entries. Match semantics are preserved:
/// `*` does not cross `/`, `**` does (globset `literal_separator(true)`),
/// matched against the entry path relative to the walk root.
fn run_glob_legacy(
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    pattern: String,
    limit: usize,
) -> Result<Vec<String>> {
    let pattern_path = PathBuf::from(&pattern);
    let host_absolute = filesystem_scope.is_host() && pattern_path.is_absolute();

    // Compute the walk root and the remaining glob pattern. Host-absolute
    // patterns anchor at the absolute non-glob prefix; everything else
    // anchors under `base_dir`.
    let (prefix, remainder) = split_glob_prefix(&pattern);
    let (root, glob_pattern): (PathBuf, String) = if host_absolute {
        let prefix_path = if prefix.is_empty() {
            PathBuf::from("/")
        } else {
            PathBuf::from(&prefix)
        };
        (prefix_path, remainder)
    } else if prefix.is_empty() {
        (base_dir.clone(), remainder)
    } else {
        (base_dir.join(&prefix), remainder)
    };

    let mut files: Vec<String> = Vec::new();

    // Literal-only pattern (no metachars): treat the root as a single match
    // if it is an existing file, mirroring `glob::glob` (which returns the
    // literal path when it exists).
    if glob_pattern.is_empty() {
        if root.is_file() {
            let display = root
                .strip_prefix(&base_dir)
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| root.clone());
            files.push(display.display().to_string());
        }
        return Ok(files);
    }

    let glob = GlobBuilder::new(&glob_pattern)
        .literal_separator(true)
        .build()
        .wrap_err_with(|| format!("invalid glob pattern: {glob_pattern}"))?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    let glob_set = builder.build().wrap_err("globset build failed")?;

    let walk_root = root.clone();
    let walker = WalkBuilder::new(&root)
        .follow_links(false)
        .hidden(false)
        .git_ignore(false)
        .filter_entry(move |e| should_descend(e, &walk_root))
        .build();

    for entry in walker {
        if files.len() >= limit {
            // FIX 2: log ONCE on truncation (no silent unbounded scans).
            tracing::info!(limit, "glob result truncated at limit");
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // FIX 2: unreadable/permission-denied subtree — debug, never
                // a per-entry WARN (the mini5 flood).
                tracing::debug!(error = %e, "glob entry skipped (unreadable)");
                continue;
            }
        };
        let path = entry.path();

        if path == root {
            continue;
        }
        if path.is_dir() {
            continue;
        }

        let rel = match path.strip_prefix(&root) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if !glob_set.is_match(rel) {
            continue;
        }

        let display_path = path
            .strip_prefix(&base_dir)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.to_path_buf());
        files.push(display_path.display().to_string());
    }

    Ok(files)
}
