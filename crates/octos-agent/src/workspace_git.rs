use std::path::{Path, PathBuf};
use std::process::Command;

use eyre::{eyre, Result, WrapErr};

use crate::workspace_policy::{
    read_workspace_policy, WorkspacePolicyKind, WorkspaceSnapshotTrigger,
    WorkspaceVersionControlProvider,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceProjectKind {
    Slides,
    Sites,
}

impl WorkspaceProjectKind {
    fn display_name(self) -> &'static str {
        match self {
            Self::Slides => "slides",
            Self::Sites => "site",
        }
    }

    fn directory_name(self) -> &'static str {
        match self {
            Self::Slides => "slides",
            Self::Sites => "sites",
        }
    }

    fn gitignore_template(self) -> &'static str {
        match self {
            Self::Slides => "/history/\n/output/\n/skill-output/\n*.pptx\n*.tmp\n.DS_Store\n",
            Self::Sites => {
                "/node_modules/\n/dist/\n/out/\n/docs/\n/build/\n/.astro/\n/.next/\n/.quarto/\n*.log\n.DS_Store\n"
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRepo {
    pub kind: WorkspaceProjectKind,
    pub root: PathBuf,
    pub slug: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WorkspaceTurnSnapshotReport {
    pub committed: Vec<String>,
    pub enforced_failures: Vec<WorkspaceTurnSnapshotFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceTurnSnapshotFailure {
    pub repo_label: String,
    pub error: String,
}

enum WorkspaceTurnSnapshotPlan {
    LegacyGit,
    PolicyGit {
        auto_init: bool,
        fail_on_error: bool,
    },
    Skip,
}

pub fn detect_workspace_repo(base_dir: &Path, changed_path: &Path) -> Option<WorkspaceRepo> {
    let relative = changed_path.strip_prefix(base_dir).ok()?;
    let mut components = relative.components();
    let category = components.next()?.as_os_str().to_str()?;
    let slug = components.next()?.as_os_str().to_str()?.to_string();
    let kind = match category {
        "slides" => WorkspaceProjectKind::Slides,
        "sites" => WorkspaceProjectKind::Sites,
        _ => return None,
    };

    Some(WorkspaceRepo {
        kind,
        root: base_dir.join(category).join(&slug),
        slug,
    })
}

pub fn init_workspace_repo(project_root: &Path, kind: WorkspaceProjectKind) -> Result<()> {
    std::fs::create_dir_all(project_root)
        .wrap_err_with(|| format!("create project dir failed: {}", project_root.display()))?;

    let gitignore_path = project_root.join(".gitignore");
    if !gitignore_path.exists() {
        std::fs::write(&gitignore_path, kind.gitignore_template())
            .wrap_err_with(|| format!("write .gitignore failed: {}", gitignore_path.display()))?;
    }

    if !project_root.join(".git").exists() {
        run_git(project_root, &["init"])?;
    }

    ensure_local_identity(project_root)?;
    Ok(())
}

pub fn commit_all_if_dirty(project_root: &Path, message: &str) -> Result<bool> {
    commit_all_if_dirty_with_options(
        project_root,
        infer_kind_from_root(project_root)?,
        message,
        true,
    )
}

pub fn initialize_and_commit(
    project_root: &Path,
    kind: WorkspaceProjectKind,
    message: &str,
) -> Result<bool> {
    init_workspace_repo(project_root, kind)?;
    run_git(project_root, &["add", "-A", "--", "."])?;

    let status = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["diff", "--cached", "--quiet", "--", "."])
        .status()
        .wrap_err("git diff --cached failed")?;

    if status.success() {
        return Ok(false);
    }

    run_git(project_root, &["commit", "-m", message, "--no-verify"])?;
    Ok(true)
}

pub fn snapshot_workspace_change(
    base_dir: &Path,
    changed_path: &Path,
    operation: &str,
) -> Result<Option<String>> {
    let repo = match detect_workspace_repo(base_dir, changed_path) {
        Some(repo) => repo,
        None => return Ok(None),
    };

    init_workspace_repo(&repo.root, repo.kind)?;

    let relative_path = changed_path
        .strip_prefix(&repo.root)
        .unwrap_or(changed_path)
        .display()
        .to_string();
    let message = format!(
        "Update {} via {}: {}",
        repo.kind.display_name(),
        operation,
        relative_path
    );

    if commit_all_if_dirty(&repo.root, &message)? {
        Ok(Some(message))
    } else {
        Ok(None)
    }
}

pub fn list_workspace_repos(base_dir: &Path) -> Result<Vec<WorkspaceRepo>> {
    let mut repos = Vec::new();

    for (category, kind) in [
        ("slides", WorkspaceProjectKind::Slides),
        ("sites", WorkspaceProjectKind::Sites),
    ] {
        let category_dir = base_dir.join(category);
        if !category_dir.exists() {
            continue;
        }

        for entry in std::fs::read_dir(&category_dir).wrap_err_with(|| {
            format!("read workspace category failed: {}", category_dir.display())
        })? {
            let entry = entry.wrap_err_with(|| {
                format!(
                    "read workspace repo entry failed: {}",
                    category_dir.display()
                )
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let slug = entry.file_name().to_string_lossy().to_string();
            repos.push(WorkspaceRepo {
                kind,
                root: path,
                slug,
            });
        }
    }

    repos.sort_by(|a, b| {
        a.kind
            .display_name()
            .cmp(b.kind.display_name())
            .then_with(|| a.slug.cmp(&b.slug))
    });
    Ok(repos)
}

pub fn snapshot_workspace_turn(
    base_dir: &Path,
    summary: &str,
) -> Result<WorkspaceTurnSnapshotReport> {
    let repos = list_workspace_repos(base_dir)?;
    let mut report = WorkspaceTurnSnapshotReport::default();
    let summary = normalize_turn_summary(summary);

    for repo in repos {
        let repo_label = format!("{}/{}", repo.kind.directory_name(), repo.slug);
        let message = format!("Turn snapshot for {repo_label}: {summary}");
        match snapshot_plan_for_repo(&repo) {
            Ok(WorkspaceTurnSnapshotPlan::Skip) => {}
            Ok(WorkspaceTurnSnapshotPlan::LegacyGit) => {
                if commit_all_if_dirty(&repo.root, &message)? {
                    report.committed.push(repo_label);
                }
            }
            Ok(WorkspaceTurnSnapshotPlan::PolicyGit {
                auto_init,
                fail_on_error,
            }) => {
                match commit_all_if_dirty_with_options(&repo.root, repo.kind, &message, auto_init) {
                    Ok(true) => report.committed.push(repo_label),
                    Ok(false) => {}
                    Err(error) => {
                        if fail_on_error {
                            report.enforced_failures.push(WorkspaceTurnSnapshotFailure {
                                repo_label,
                                error: error.to_string(),
                            });
                        }
                    }
                }
            }
            Err(error) => {
                report.enforced_failures.push(WorkspaceTurnSnapshotFailure {
                    repo_label,
                    error: error.to_string(),
                });
            }
        }
    }

    Ok(report)
}

fn commit_all_if_dirty_with_options(
    project_root: &Path,
    kind: WorkspaceProjectKind,
    message: &str,
    auto_init: bool,
) -> Result<bool> {
    if auto_init {
        init_workspace_repo(project_root, kind).wrap_err("ensure git repo failed")?;
    } else if !project_root.join(".git").exists() {
        return Err(eyre!(
            "workspace policy requires git repo at {}, but auto_init is disabled",
            project_root.display()
        ));
    }

    run_git(project_root, &["add", "-A", "--", "."])?;

    let status = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["diff", "--cached", "--quiet", "--", "."])
        .status()
        .wrap_err("git diff --cached failed")?;

    if status.success() {
        return Ok(false);
    }

    run_git(project_root, &["commit", "-m", message, "--no-verify"])?;
    Ok(true)
}

fn snapshot_plan_for_repo(repo: &WorkspaceRepo) -> Result<WorkspaceTurnSnapshotPlan> {
    let Some(policy) = read_workspace_policy(&repo.root)? else {
        return Ok(WorkspaceTurnSnapshotPlan::LegacyGit);
    };

    if !policy.workspace.kind.matches_project_kind(repo.kind) {
        return Err(eyre!(
            "workspace policy kind mismatch for {}: expected {}, found {}",
            repo.root.display(),
            WorkspacePolicyKind::from(repo.kind).as_str(),
            policy.workspace.kind.as_str(),
        ));
    }

    if policy.version_control.provider != WorkspaceVersionControlProvider::Git {
        return Ok(WorkspaceTurnSnapshotPlan::Skip);
    }

    if policy.version_control.trigger != WorkspaceSnapshotTrigger::TurnEnd {
        return Ok(WorkspaceTurnSnapshotPlan::Skip);
    }

    Ok(WorkspaceTurnSnapshotPlan::PolicyGit {
        auto_init: policy.version_control.auto_init,
        fail_on_error: policy.version_control.fail_on_error,
    })
}

fn infer_kind_from_root(project_root: &Path) -> Result<WorkspaceProjectKind> {
    let parent = project_root
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            eyre!(
                "cannot infer workspace project kind from {}",
                project_root.display()
            )
        })?;

    match parent {
        "slides" => Ok(WorkspaceProjectKind::Slides),
        "sites" => Ok(WorkspaceProjectKind::Sites),
        _ => Err(eyre!(
            "unsupported workspace project root for git snapshot: {}",
            project_root.display()
        )),
    }
}

fn ensure_local_identity(project_root: &Path) -> Result<()> {
    run_git(
        project_root,
        &["config", "--local", "user.name", "Octos Workspace"],
    )?;
    run_git(
        project_root,
        &["config", "--local", "user.email", "octos@local"],
    )?;
    Ok(())
}

fn normalize_turn_summary(summary: &str) -> String {
    let compact = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let fallback = if compact.is_empty() {
        "update workspace".to_string()
    } else {
        compact
    };

    truncate_utf8_boundary(&fallback, 72)
}

fn truncate_utf8_boundary(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }

    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

fn run_git(project_root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(args)
        .output()
        .wrap_err_with(|| format!("failed to spawn git {:?}", args))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(eyre!(
        "git {:?} failed in {}: {}{}",
        args,
        project_root.display(),
        stderr.trim(),
        if stdout.trim().is_empty() {
            String::new()
        } else {
            format!(" | stdout: {}", stdout.trim())
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_policy::{write_workspace_policy, WorkspacePolicy};

    #[test]
    fn detects_slides_repo_from_changed_path() {
        let base = Path::new("/tmp/workspace");
        let changed = base.join("slides/demo/script.js");
        let repo = detect_workspace_repo(base, &changed).expect("repo");
        assert_eq!(repo.kind, WorkspaceProjectKind::Slides);
        assert_eq!(repo.root, base.join("slides/demo"));
        assert_eq!(repo.slug, "demo");
    }

    #[test]
    fn ignores_non_project_paths() {
        let base = Path::new("/tmp/workspace");
        let changed = base.join("skill-output/demo/file.txt");
        assert!(detect_workspace_repo(base, &changed).is_none());
    }

    #[test]
    fn initializes_and_commits_repo() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("slides").join("deck");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(project_root.join("script.js"), "module.exports = [];\n").unwrap();

        let committed = initialize_and_commit(
            &project_root,
            WorkspaceProjectKind::Slides,
            "Initialize slides workspace",
        )
        .unwrap();

        assert!(committed);
        assert!(project_root.join(".git").exists());
        assert!(project_root.join(".gitignore").exists());
    }

    #[test]
    fn lists_workspace_repos_by_category() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("slides").join("deck-a")).unwrap();
        std::fs::create_dir_all(temp.path().join("sites").join("newsbot")).unwrap();

        let repos = list_workspace_repos(temp.path()).unwrap();
        let labels: Vec<String> = repos
            .iter()
            .map(|repo| format!("{}/{}", repo.kind.directory_name(), repo.slug))
            .collect();

        assert_eq!(labels, vec!["sites/newsbot", "slides/deck-a"]);
    }

    #[test]
    fn snapshots_all_dirty_workspace_repos() {
        let temp = tempfile::tempdir().unwrap();
        let slides_root = temp.path().join("slides").join("deck-a");
        let sites_root = temp.path().join("sites").join("newsbot");
        std::fs::create_dir_all(&slides_root).unwrap();
        std::fs::create_dir_all(&sites_root).unwrap();
        std::fs::write(slides_root.join("script.js"), "module.exports = [];\n").unwrap();
        std::fs::write(sites_root.join("index.html"), "<h1>hello</h1>\n").unwrap();
        write_workspace_policy(
            &slides_root,
            &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
        )
        .unwrap();
        write_workspace_policy(
            &sites_root,
            &WorkspacePolicy::for_kind(WorkspaceProjectKind::Sites),
        )
        .unwrap();

        let report = snapshot_workspace_turn(temp.path(), "apply user request").unwrap();

        assert_eq!(report.committed, vec!["sites/newsbot", "slides/deck-a"]);
        assert!(report.enforced_failures.is_empty());
        assert!(slides_root.join(".git").exists());
        assert!(sites_root.join(".git").exists());
    }

    #[test]
    fn reports_malformed_policy_as_enforced_failure() {
        let temp = tempfile::tempdir().unwrap();
        let slides_root = temp.path().join("slides").join("deck-a");
        std::fs::create_dir_all(&slides_root).unwrap();
        std::fs::write(slides_root.join("script.js"), "module.exports = [];\n").unwrap();
        std::fs::write(
            slides_root.join(".octos-workspace.toml"),
            "[workspace]\nkind = \"slides\"\n[version_control]\nprovider = ",
        )
        .unwrap();

        let report = snapshot_workspace_turn(temp.path(), "apply user request").unwrap();

        assert!(report.committed.is_empty());
        assert_eq!(report.enforced_failures.len(), 1);
        assert_eq!(report.enforced_failures[0].repo_label, "slides/deck-a");
        assert!(report.enforced_failures[0]
            .error
            .contains("parse workspace policy failed"));
    }
}
