//! Read-only workspace git history tools.
//!
//! Three tools that give the agent visibility into workspace git history
//! without modifying it. All run server-side outside the sandbox.
//!
//! - `workspace_log`: git log for a workspace project
//! - `workspace_show`: read a file at a specific commit
//! - `workspace_diff`: diff between two commits for a file

use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;

use super::{Tool, ToolResult};
use crate::workspace_git::detect_workspace_repo;

/// Max output length for git commands (50KB).
const MAX_OUTPUT: usize = 50_000;

/// Run a git command in a directory and return stdout as a string.
fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .wrap_err_with(|| format!("failed to run git {:?}", args))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre::eyre!("git {:?} failed: {}", args, stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Resolve a workspace project path from a relative slides/sites path.
/// Returns the project root if it has a .git directory.
fn resolve_workspace_git_root(base_dir: &Path, project_path: &str) -> Result<PathBuf> {
    let full = base_dir.join(project_path);

    // Must be under base_dir
    let canonical_base = base_dir
        .canonicalize()
        .unwrap_or_else(|_| base_dir.to_path_buf());
    let canonical_full = full.canonicalize().unwrap_or_else(|_| full.clone());
    if !canonical_full.starts_with(&canonical_base) {
        return Err(eyre::eyre!("path outside workspace: {project_path}"));
    }

    // Detect workspace repo from the path
    if let Some(repo) = detect_workspace_repo(base_dir, &full) {
        if repo.root.join(".git").exists() {
            return Ok(repo.root);
        }
    }

    // Fallback: check if the path itself is a git repo
    if full.join(".git").exists() {
        return Ok(full);
    }

    Err(eyre::eyre!(
        "no git repository found at {project_path}. Use a path like slides/<name> or sites/<name>."
    ))
}

// ── workspace_log ──────────────────────────────────────────────────

/// Tool: view git commit history for a workspace project.
pub struct WorkspaceLogTool {
    base_dir: PathBuf,
}

impl WorkspaceLogTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkspaceLogInput {
    /// Workspace project path, e.g. "slides/my-deck" or "sites/blog".
    project: String,
    /// Max number of commits to show (default 20).
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    20
}

#[async_trait]
impl Tool for WorkspaceLogTool {
    fn name(&self) -> &str {
        "workspace_log"
    }

    fn description(&self) -> &str {
        "View git commit history for a workspace project (slides or sites). Shows recent commits with hashes you can use with workspace_show and workspace_diff."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "workspace"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Workspace project path, e.g. 'slides/my-deck' or 'sites/blog'"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max commits to show (default 20)"
                }
            },
            "required": ["project"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: WorkspaceLogInput =
            serde_json::from_value(args.clone()).wrap_err("invalid workspace_log input")?;

        let repo_root = match resolve_workspace_git_root(&self.base_dir, &input.project) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult {
                    output: e.to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let limit_str = input.limit.min(100).to_string();
        let mut output = match run_git(&repo_root, &["log", "--oneline", "--all", "-n", &limit_str])
        {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("git log failed: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        if output.is_empty() {
            output = "(no commits yet)".to_string();
        }

        octos_core::truncate_utf8(&mut output, MAX_OUTPUT, "\n... (truncated)");

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }
}

// ── workspace_show ─────────────────────────────────────────────────

/// Tool: read a file at a specific commit in workspace git history.
pub struct WorkspaceShowTool {
    base_dir: PathBuf,
}

impl WorkspaceShowTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkspaceShowInput {
    /// Workspace project path, e.g. "slides/my-deck".
    project: String,
    /// Git commit hash (short or full).
    commit: String,
    /// File path relative to project root, e.g. "script.js".
    file: String,
}

#[async_trait]
impl Tool for WorkspaceShowTool {
    fn name(&self) -> &str {
        "workspace_show"
    }

    fn description(&self) -> &str {
        "Read the contents of a file at a specific git commit in a workspace project. Use workspace_log first to find commit hashes."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "workspace"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Workspace project path, e.g. 'slides/my-deck'"
                },
                "commit": {
                    "type": "string",
                    "description": "Git commit hash (from workspace_log)"
                },
                "file": {
                    "type": "string",
                    "description": "File path relative to project root, e.g. 'script.js'"
                }
            },
            "required": ["project", "commit", "file"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: WorkspaceShowInput =
            serde_json::from_value(args.clone()).wrap_err("invalid workspace_show input")?;

        // Reject commit hashes with path traversal
        if input.commit.contains("..") || input.commit.contains('/') {
            return Ok(ToolResult {
                output: "invalid commit hash".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // Reject file paths with traversal
        if input.file.contains("..") {
            return Ok(ToolResult {
                output: "path traversal not allowed".to_string(),
                success: false,
                ..Default::default()
            });
        }

        let repo_root = match resolve_workspace_git_root(&self.base_dir, &input.project) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult {
                    output: e.to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let ref_spec = format!("{}:{}", input.commit, input.file);
        let mut output = match run_git(&repo_root, &["show", &ref_spec]) {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("git show failed: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        octos_core::truncate_utf8(&mut output, MAX_OUTPUT, "\n... (truncated)");

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }
}

// ── workspace_diff ─────────────────────────────────────────────────

/// Tool: show what changed between two commits for a file.
pub struct WorkspaceDiffTool {
    base_dir: PathBuf,
}

impl WorkspaceDiffTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkspaceDiffInput {
    /// Workspace project path, e.g. "slides/my-deck".
    project: String,
    /// Older commit hash.
    from_commit: String,
    /// Newer commit hash (or "HEAD" for current).
    #[serde(default = "default_head")]
    to_commit: String,
    /// Optional file path to scope the diff.
    #[serde(default)]
    file: Option<String>,
}

fn default_head() -> String {
    "HEAD".to_string()
}

#[async_trait]
impl Tool for WorkspaceDiffTool {
    fn name(&self) -> &str {
        "workspace_diff"
    }

    fn description(&self) -> &str {
        "Show what changed between two commits in a workspace project. Optionally scope to a specific file."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "workspace"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Workspace project path, e.g. 'slides/my-deck'"
                },
                "from_commit": {
                    "type": "string",
                    "description": "Older commit hash (from workspace_log)"
                },
                "to_commit": {
                    "type": "string",
                    "description": "Newer commit hash, or 'HEAD' for current (default: HEAD)"
                },
                "file": {
                    "type": "string",
                    "description": "Optional file path to scope the diff"
                }
            },
            "required": ["project", "from_commit"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: WorkspaceDiffInput =
            serde_json::from_value(args.clone()).wrap_err("invalid workspace_diff input")?;

        // Reject traversal in commit refs
        for ref_str in [&input.from_commit, &input.to_commit] {
            if ref_str.contains('/') && !ref_str.starts_with("HEAD") {
                return Ok(ToolResult {
                    output: "invalid commit ref".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        }

        if let Some(ref f) = input.file {
            if f.contains("..") {
                return Ok(ToolResult {
                    output: "path traversal not allowed".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        }

        let repo_root = match resolve_workspace_git_root(&self.base_dir, &input.project) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult {
                    output: e.to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let range = format!("{}..{}", input.from_commit, input.to_commit);
        let mut git_args = vec!["diff", &range, "--"];
        if let Some(ref f) = input.file {
            git_args.push(f);
        }

        let mut output = match run_git(&repo_root, &git_args) {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("git diff failed: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        if output.is_empty() {
            output = "(no changes between these commits)".to_string();
        }

        octos_core::truncate_utf8(&mut output, MAX_OUTPUT, "\n... (truncated)");

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

    /// Set up a git repo with 3 commits for testing.
    fn setup_test_repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let slides = temp.path().join("slides").join("test-deck");
        std::fs::create_dir_all(&slides).unwrap();

        // git init
        Command::new("git")
            .args(["init"])
            .current_dir(&slides)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&slides)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test"])
            .current_dir(&slides)
            .output()
            .unwrap();

        // Commit 1: initial script.js
        std::fs::write(
            slides.join("script.js"),
            "// v1\nslide 1\nslide 2\nslide 3\n",
        )
        .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&slides)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial script", "--no-verify"])
            .current_dir(&slides)
            .output()
            .unwrap();

        // Commit 2: edit slide 2
        std::fs::write(
            slides.join("script.js"),
            "// v2\nslide 1\nslide 2 EDITED\nslide 3\n",
        )
        .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&slides)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Edit slide 2", "--no-verify"])
            .current_dir(&slides)
            .output()
            .unwrap();

        // Commit 3: delete slide 3
        std::fs::write(slides.join("script.js"), "// v3\nslide 1\nslide 2 EDITED\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&slides)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Delete slide 3", "--no-verify"])
            .current_dir(&slides)
            .output()
            .unwrap();

        temp
    }

    // ── workspace_log tests ────────────────────────────────────────

    #[tokio::test]
    async fn should_show_commit_history() {
        let temp = setup_test_repo();
        let tool = WorkspaceLogTool::new(temp.path());

        let result = tool
            .execute(&serde_json::json!({"project": "slides/test-deck"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("Delete slide 3"));
        assert!(result.output.contains("Edit slide 2"));
        assert!(result.output.contains("Initial script"));
    }

    #[tokio::test]
    async fn should_limit_commit_count() {
        let temp = setup_test_repo();
        let tool = WorkspaceLogTool::new(temp.path());

        let result = tool
            .execute(&serde_json::json!({"project": "slides/test-deck", "limit": 1}))
            .await
            .unwrap();

        assert!(result.success);
        let lines: Vec<&str> = result.output.trim().lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(result.output.contains("Delete slide 3"));
    }

    #[tokio::test]
    async fn should_fail_for_missing_project() {
        let temp = tempfile::tempdir().unwrap();
        let tool = WorkspaceLogTool::new(temp.path());

        let result = tool
            .execute(&serde_json::json!({"project": "slides/nonexistent"}))
            .await
            .unwrap();

        assert!(!result.success);
        // Error may be "no git repository" or "path outside workspace" depending
        // on whether the path exists on disk for canonicalization.
        assert!(
            result.output.contains("no git repository")
                || result.output.contains("path outside workspace"),
            "unexpected error: {}",
            result.output
        );
    }

    // ── workspace_show tests ───────────────────────────────────────

    #[tokio::test]
    async fn should_show_file_at_old_commit() {
        let temp = setup_test_repo();
        let tool = WorkspaceShowTool::new(temp.path());

        // Get first commit hash
        let log_output = run_git(
            &temp.path().join("slides/test-deck"),
            &["log", "--oneline", "--reverse"],
        )
        .unwrap();
        let first_hash = log_output
            .lines()
            .next()
            .unwrap()
            .split(' ')
            .next()
            .unwrap();

        let result = tool
            .execute(&serde_json::json!({
                "project": "slides/test-deck",
                "commit": first_hash,
                "file": "script.js"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("// v1"));
        assert!(result.output.contains("slide 3")); // v1 still had slide 3
        assert!(!result.output.contains("EDITED")); // v1 didn't have the edit
    }

    #[tokio::test]
    async fn should_show_current_version_at_head() {
        let temp = setup_test_repo();
        let tool = WorkspaceShowTool::new(temp.path());

        let result = tool
            .execute(&serde_json::json!({
                "project": "slides/test-deck",
                "commit": "HEAD",
                "file": "script.js"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("// v3"));
        assert!(!result.output.contains("slide 3")); // v3 deleted slide 3
    }

    #[tokio::test]
    async fn should_reject_path_traversal_in_commit() {
        let temp = setup_test_repo();
        let tool = WorkspaceShowTool::new(temp.path());

        let result = tool
            .execute(&serde_json::json!({
                "project": "slides/test-deck",
                "commit": "../../etc/passwd",
                "file": "script.js"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("invalid commit hash"));
    }

    #[tokio::test]
    async fn should_reject_path_traversal_in_file() {
        let temp = setup_test_repo();
        let tool = WorkspaceShowTool::new(temp.path());

        let result = tool
            .execute(&serde_json::json!({
                "project": "slides/test-deck",
                "commit": "HEAD",
                "file": "../../etc/passwd"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("path traversal not allowed"));
    }

    #[tokio::test]
    async fn should_fail_for_nonexistent_file_at_commit() {
        let temp = setup_test_repo();
        let tool = WorkspaceShowTool::new(temp.path());

        let result = tool
            .execute(&serde_json::json!({
                "project": "slides/test-deck",
                "commit": "HEAD",
                "file": "nonexistent.js"
            }))
            .await
            .unwrap();

        assert!(!result.success);
    }

    // ── workspace_diff tests ───────────────────────────────────────

    #[tokio::test]
    async fn should_diff_between_two_commits() {
        let temp = setup_test_repo();
        let tool = WorkspaceDiffTool::new(temp.path());

        // Get first and last commit hashes
        let log_output = run_git(
            &temp.path().join("slides/test-deck"),
            &["log", "--oneline", "--reverse"],
        )
        .unwrap();
        let hashes: Vec<&str> = log_output
            .lines()
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        let first = hashes[0];
        let last = hashes[2];

        let result = tool
            .execute(&serde_json::json!({
                "project": "slides/test-deck",
                "from_commit": first,
                "to_commit": last,
                "file": "script.js"
            }))
            .await
            .unwrap();

        assert!(result.success);
        // Diff should show changes from v1 to v3
        assert!(result.output.contains("-// v1"));
        assert!(result.output.contains("+// v3"));
        assert!(result.output.contains("-slide 3")); // slide 3 was removed
    }

    #[tokio::test]
    async fn should_diff_to_head_by_default() {
        let temp = setup_test_repo();
        let tool = WorkspaceDiffTool::new(temp.path());

        let log_output = run_git(
            &temp.path().join("slides/test-deck"),
            &["log", "--oneline", "--reverse"],
        )
        .unwrap();
        let first = log_output
            .lines()
            .next()
            .unwrap()
            .split(' ')
            .next()
            .unwrap();

        let result = tool
            .execute(&serde_json::json!({
                "project": "slides/test-deck",
                "from_commit": first
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("-// v1"));
        assert!(result.output.contains("+// v3"));
    }

    #[tokio::test]
    async fn should_show_no_changes_for_same_commit() {
        let temp = setup_test_repo();
        let tool = WorkspaceDiffTool::new(temp.path());

        let result = tool
            .execute(&serde_json::json!({
                "project": "slides/test-deck",
                "from_commit": "HEAD",
                "to_commit": "HEAD"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("no changes"));
    }

    // ── tool metadata ──────────────────────────────────────────────

    #[test]
    fn should_have_correct_tool_names() {
        let log = WorkspaceLogTool::new("/tmp");
        let show = WorkspaceShowTool::new("/tmp");
        let diff = WorkspaceDiffTool::new("/tmp");

        assert_eq!(log.name(), "workspace_log");
        assert_eq!(show.name(), "workspace_show");
        assert_eq!(diff.name(), "workspace_diff");

        assert!(log.tags().contains(&"workspace"));
        assert!(show.tags().contains(&"workspace"));
        assert!(diff.tags().contains(&"workspace"));
    }
}
