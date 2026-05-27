//! Glob tool for finding files by pattern.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
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

        // PR-B: when a `SessionScope` is present, the host trusts the
        // scope to be the single source of truth for path containment.
        // Absolute patterns inside any allowed zone (workspace,
        // granted_dirs, shared_zones, skill_read_zones) are accepted;
        // results are canonicalised and dropped if they classify
        // `OutOfScope`. Without a scope we keep the legacy
        // base_dir + FilesystemScope policy (back-compat for `octos
        // chat`).
        if ctx.session_scope.is_none()
            && !filesystem_scope.is_host()
            && (pattern.starts_with('/') || pattern.contains(".."))
        {
            return Ok(ToolResult {
                output: "Absolute paths and '..' are not allowed in glob patterns".to_string(),
                success: false,
                ..Default::default()
            });
        }

        let scope = ctx.session_scope.clone();
        let base_dir = self.base_dir.clone();
        let pattern_clone = pattern.clone();

        // Run glob in blocking task.
        let result = tokio::task::spawn_blocking(move || {
            run_glob(scope, base_dir, filesystem_scope, pattern_clone, limit)
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

/// Run the glob and filter results, blocking-thread side.
fn run_glob(
    scope: Option<Arc<SessionScope>>,
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    pattern: String,
    limit: usize,
) -> Result<Vec<String>> {
    // Anchor pattern: when scope is present, relative patterns resolve
    // against `scope.workspace()`; absolute patterns are taken
    // verbatim. Without scope, fall through to the legacy
    // base_dir-anchored behaviour (host-scope passes absolute through).
    let pattern_path = PathBuf::from(&pattern);
    let full_pattern = match scope.as_ref() {
        Some(scope) => {
            if pattern_path.is_absolute() {
                pattern.clone()
            } else {
                format!("{}/{}", scope.workspace().display(), pattern)
            }
        }
        None => {
            if filesystem_scope.is_host() && pattern_path.is_absolute() {
                pattern.clone()
            } else {
                format!("{}/{}", base_dir.display(), pattern)
            }
        }
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

        // PR-B: when a scope is present, drop any match whose
        // canonicalised path falls outside every declared zone. This
        // also closes the ancestor-symlink hole: a symlink inside the
        // workspace pointing at `/etc` would have surfaced
        // `<workspace>/link/passwd` as `InWorkspace` under the lexical
        // classifier, but canonicalize-then-classify rejects it.
        if let Some(scope) = scope.as_ref() {
            if matches!(
                scope.classify_canonical_path(&path),
                PathClassification::OutOfScope
            ) {
                continue;
            }
        }

        // Display path: prefer relative to the configured anchor.
        // With scope: relative to `scope.workspace()`. Otherwise:
        // relative to base_dir (the legacy display).
        let display_anchor = scope
            .as_ref()
            .map(|s| s.workspace().to_path_buf())
            .unwrap_or_else(|| base_dir.clone());
        let display_path = path
            .strip_prefix(&display_anchor)
            .map(|p| p.to_path_buf())
            .unwrap_or(path);
        files.push(display_path.display().to_string());
    }

    Ok(files)
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

    // ------------------------------------------------------------------
    // PR-B: SessionScope integration tests for GlobTool.
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
        // The legacy `..`-rejection still applies — the rejection
        // wins lexically before the glob even runs.
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("passwd"), "root:x:0:0").unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
        let tool = GlobTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        // An absolute glob to a non-zone path: pattern is accepted at
        // input time (we trust the scope), but every match is
        // canonicalised and dropped as `OutOfScope`.
        let pattern = format!("{}/*", outside.path().display());
        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"pattern": pattern}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            result.output.contains("No files found"),
            "expected zero matches (all classify OutOfScope), got: {}",
            result.output
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_drops_symlink_target_outside_scope() {
        // A symlink inside the workspace pointing at /etc would
        // otherwise let `<workspace>/link/passwd` masquerade as
        // workspace-resident. The canonical filter drops it.
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
}
