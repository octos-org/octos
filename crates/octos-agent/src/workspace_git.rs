use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use glob::glob;
use serde::Serialize;
use tracing::warn;

use crate::behaviour::{ActionContext, ActionResult, evaluate_actions_with_context};
use crate::workspace_policy::{
    WorkspacePolicy, WorkspacePolicyKind, WorkspaceSnapshotTrigger,
    WorkspaceVersionControlProvider, read_workspace_policy,
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
    pub validation_failures: Vec<WorkspaceValidationFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceValidationFailure {
    pub repo_label: String,
    pub phase: WorkspaceValidationPhase,
    pub check: String,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceValidationPhase {
    TurnEnd,
    Completion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceTurnSnapshotFailure {
    pub repo_label: String,
    pub error: String,
}

#[derive(Clone, Debug)]
struct PendingValidation {
    repo_root: PathBuf,
    repo_label: String,
    specs: Vec<String>,
    context: ActionContext,
}

enum WorkspaceTurnSnapshotPlan {
    LegacyGit,
    PolicyGit {
        auto_init: bool,
        fail_on_error: bool,
        turn_end_validators: Vec<String>,
        completion_validators: Vec<String>,
        artifact_patterns: Vec<String>,
        artifact_context: ActionContext,
    },
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceContractStatus {
    pub repo_label: String,
    pub kind: String,
    pub slug: String,
    pub policy_managed: bool,
    pub revision: Option<String>,
    pub dirty: bool,
    pub ready: bool,
    pub error: Option<String>,
    pub turn_end_checks: Vec<WorkspaceCheckStatus>,
    pub completion_checks: Vec<WorkspaceCheckStatus>,
    pub artifacts: Vec<WorkspaceArtifactStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceCheckStatus {
    pub spec: String,
    pub passed: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceArtifactStatus {
    pub name: String,
    pub pattern: String,
    pub present: bool,
    pub matches: Vec<String>,
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
    with_repo_git_lock(project_root, || {
        init_workspace_repo_unlocked(project_root, kind)
    })
}

fn init_workspace_repo_unlocked(project_root: &Path, kind: WorkspaceProjectKind) -> Result<()> {
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
    with_repo_git_lock(project_root, || {
        init_workspace_repo_unlocked(project_root, kind)?;
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
    })
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

    // Collect (repo_root, validators) from the plan phase so we don't re-read
    // the workspace policy during validation.
    let mut pending_turn_end_validations: Vec<PendingValidation> = Vec::new();
    let mut pending_completion_validations: Vec<PendingValidation> = Vec::new();

    for repo in &repos {
        let repo_label = format!("{}/{}", repo.kind.directory_name(), repo.slug);
        let message = format!("Turn snapshot for {repo_label}: {summary}");
        match snapshot_plan_for_repo(repo) {
            Ok(WorkspaceTurnSnapshotPlan::Skip) => {}
            Ok(WorkspaceTurnSnapshotPlan::LegacyGit) => {
                if commit_all_if_dirty(&repo.root, &message)? {
                    report.committed.push(repo_label);
                }
            }
            Ok(WorkspaceTurnSnapshotPlan::PolicyGit {
                auto_init,
                fail_on_error,
                turn_end_validators,
                completion_validators,
                artifact_patterns,
                artifact_context,
            }) => {
                match commit_all_if_dirty_with_options(&repo.root, repo.kind, &message, auto_init) {
                    Ok(true) => report.committed.push(repo_label.clone()),
                    Ok(false) => {}
                    Err(error) => {
                        if fail_on_error {
                            report.enforced_failures.push(WorkspaceTurnSnapshotFailure {
                                repo_label: repo_label.clone(),
                                error: error.to_string(),
                            });
                        }
                    }
                }
                if !turn_end_validators.is_empty() {
                    pending_turn_end_validations.push(PendingValidation {
                        repo_root: repo.root.clone(),
                        repo_label: repo_label.clone(),
                        specs: turn_end_validators,
                        context: artifact_context.clone(),
                    });
                }
                if !completion_validators.is_empty()
                    && repo_has_declared_artifacts(&repo.root, &artifact_patterns)
                {
                    pending_completion_validations.push(PendingValidation {
                        repo_root: repo.root.clone(),
                        repo_label,
                        specs: completion_validators,
                        context: artifact_context.clone(),
                    });
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

    // Run turn-end validators using the already-parsed policy data.
    run_validators(
        WorkspaceValidationPhase::TurnEnd,
        &pending_turn_end_validations,
        &mut report,
    );
    run_validators(
        WorkspaceValidationPhase::Completion,
        &pending_completion_validations,
        &mut report,
    );

    Ok(report)
}

/// Run validators using pre-parsed policy data.
///
/// Each entry is `(repo_root, repo_label, validator_specs)` collected during
/// the snapshot plan phase, avoiding a second policy read from disk.
fn run_validators(
    phase: WorkspaceValidationPhase,
    validations: &[PendingValidation],
    report: &mut WorkspaceTurnSnapshotReport,
) {
    for validation in validations {
        for (spec, result) in evaluate_actions_with_context(
            &validation.repo_root,
            &validation.context,
            &validation.specs,
        ) {
            match result {
                Ok(ActionResult::Pass | ActionResult::Notify { .. }) => {}
                Ok(ActionResult::Fail { reason }) => {
                    report.validation_failures.push(WorkspaceValidationFailure {
                        repo_label: validation.repo_label.clone(),
                        phase,
                        check: spec,
                        reason,
                    });
                }
                Err(e) => {
                    warn!(
                        repo = %validation.repo_label,
                        check = %spec,
                        error = %e,
                        "turn-end validator failed to execute"
                    );
                    report.validation_failures.push(WorkspaceValidationFailure {
                        repo_label: validation.repo_label.clone(),
                        phase,
                        check: spec,
                        reason: format!("validator error: {e}"),
                    });
                }
            }
        }
    }
}

fn repo_has_declared_artifacts(repo_root: &Path, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| !resolve_artifact_matches(repo_root, pattern).is_empty())
}

fn commit_all_if_dirty_with_options(
    project_root: &Path,
    kind: WorkspaceProjectKind,
    message: &str,
    auto_init: bool,
) -> Result<bool> {
    with_repo_git_lock(project_root, || {
        if auto_init {
            init_workspace_repo_unlocked(project_root, kind).wrap_err("ensure git repo failed")?;
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
    })
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

    let artifact_context = artifact_validation_context(&repo.root, &policy.artifacts.entries);
    let artifact_patterns = policy.artifacts.entries.into_values().collect();

    Ok(WorkspaceTurnSnapshotPlan::PolicyGit {
        auto_init: policy.version_control.auto_init,
        fail_on_error: policy.version_control.fail_on_error,
        turn_end_validators: policy.validation.on_turn_end,
        completion_validators: policy.validation.on_completion,
        artifact_patterns,
        artifact_context,
    })
}

pub fn inspect_workspace_contracts(base_dir: &Path) -> Result<Vec<WorkspaceContractStatus>> {
    let repos = list_workspace_repos(base_dir)?;
    Ok(repos.iter().map(inspect_workspace_contract).collect())
}

pub fn inspect_workspace_contract(repo: &WorkspaceRepo) -> WorkspaceContractStatus {
    let repo_label = format!("{}/{}", repo.kind.directory_name(), repo.slug);
    let revision = git_head_revision(&repo.root);
    let dirty = git_is_dirty(&repo.root);

    let policy = match read_workspace_policy(&repo.root) {
        Ok(Some(policy)) => policy,
        Ok(None) => {
            return WorkspaceContractStatus {
                repo_label,
                kind: repo.kind.display_name().to_string(),
                slug: repo.slug.clone(),
                policy_managed: false,
                revision,
                dirty,
                ready: false,
                error: None,
                turn_end_checks: Vec::new(),
                completion_checks: Vec::new(),
                artifacts: Vec::new(),
            };
        }
        Err(error) => {
            return WorkspaceContractStatus {
                repo_label,
                kind: repo.kind.display_name().to_string(),
                slug: repo.slug.clone(),
                policy_managed: true,
                revision,
                dirty,
                ready: false,
                error: Some(error.to_string()),
                turn_end_checks: Vec::new(),
                completion_checks: Vec::new(),
                artifacts: Vec::new(),
            };
        }
    };

    if !policy.workspace.kind.matches_project_kind(repo.kind) {
        return WorkspaceContractStatus {
            repo_label,
            kind: repo.kind.display_name().to_string(),
            slug: repo.slug.clone(),
            policy_managed: true,
            revision,
            dirty,
            ready: false,
            error: Some(format!(
                "workspace policy kind mismatch: expected {}, found {}",
                WorkspacePolicyKind::from(repo.kind).as_str(),
                policy.workspace.kind.as_str()
            )),
            turn_end_checks: Vec::new(),
            completion_checks: Vec::new(),
            artifacts: Vec::new(),
        };
    }

    inspect_managed_workspace_contract(repo, revision, dirty, &policy)
}

fn inspect_managed_workspace_contract(
    repo: &WorkspaceRepo,
    revision: Option<String>,
    dirty: bool,
    policy: &WorkspacePolicy,
) -> WorkspaceContractStatus {
    let repo_label = format!("{}/{}", repo.kind.directory_name(), repo.slug);
    let artifact_context = artifact_validation_context(&repo.root, &policy.artifacts.entries);
    let artifacts = policy
        .artifacts
        .entries
        .iter()
        .map(|(name, pattern)| {
            let matches = resolve_artifact_matches(&repo.root, pattern);
            WorkspaceArtifactStatus {
                name: name.clone(),
                pattern: pattern.clone(),
                present: !matches.is_empty(),
                matches,
            }
        })
        .collect::<Vec<_>>();
    let turn_end_checks = evaluate_check_specs(
        &repo.root,
        &artifact_context,
        &policy.validation.on_turn_end,
    );
    let completion_checks = evaluate_check_specs(
        &repo.root,
        &artifact_context,
        &policy.validation.on_completion,
    );
    let ready = check_list_passed(&turn_end_checks)
        && check_list_passed(&completion_checks)
        && artifacts.iter().all(|artifact| artifact.present);

    WorkspaceContractStatus {
        repo_label,
        kind: repo.kind.display_name().to_string(),
        slug: repo.slug.clone(),
        policy_managed: true,
        revision,
        dirty,
        ready,
        error: None,
        turn_end_checks,
        completion_checks,
        artifacts,
    }
}

fn evaluate_check_specs(
    repo_root: &Path,
    context: &ActionContext,
    specs: &[String],
) -> Vec<WorkspaceCheckStatus> {
    evaluate_actions_with_context(repo_root, context, specs)
        .into_iter()
        .map(|(spec, result)| match result {
            Ok(ActionResult::Pass | ActionResult::Notify { .. }) => WorkspaceCheckStatus {
                spec,
                passed: true,
                reason: None,
            },
            Ok(ActionResult::Fail { reason }) => WorkspaceCheckStatus {
                spec,
                passed: false,
                reason: Some(reason),
            },
            Err(error) => WorkspaceCheckStatus {
                spec,
                passed: false,
                reason: Some(format!("validator error: {error}")),
            },
        })
        .collect()
}

fn artifact_validation_context(
    repo_root: &Path,
    artifacts: &std::collections::BTreeMap<String, String>,
) -> ActionContext {
    let named_targets = artifacts.iter().map(|(name, pattern)| {
        let matches = resolve_artifact_matches(repo_root, pattern)
            .into_iter()
            .map(|relative| repo_root.join(relative))
            .collect::<Vec<_>>();
        (format!("${name}"), matches)
    });
    ActionContext::default().with_named_targets(named_targets)
}

fn check_list_passed(checks: &[WorkspaceCheckStatus]) -> bool {
    checks.iter().all(|check| check.passed)
}

fn resolve_artifact_matches(repo_root: &Path, pattern: &str) -> Vec<String> {
    let full_pattern = if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        repo_root.join(pattern)
    };
    let canonical_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let mut matches = Vec::new();

    let Ok(entries) = glob(&full_pattern.to_string_lossy()) else {
        return matches;
    };

    for entry in entries.flatten() {
        if !entry.is_file() {
            continue;
        }
        let canonical = entry.canonicalize().unwrap_or_else(|_| entry.clone());
        if !canonical.starts_with(&canonical_root) {
            continue;
        }
        let relative = entry
            .strip_prefix(repo_root)
            .unwrap_or(&entry)
            .to_string_lossy()
            .replace('\\', "/");
        matches.push(relative);
    }

    matches.sort();
    matches.dedup();
    matches
}

fn git_head_revision(project_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!revision.is_empty()).then_some(revision)
}

fn git_is_dirty(project_root: &Path) -> bool {
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args([
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--",
            ".",
        ])
        .output()
    else {
        return false;
    };
    output.status.success() && !output.stdout.is_empty()
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
    let mut attempt = 0_u32;
    loop {
        let output = Command::new("git")
            .arg("-C")
            .arg(project_root)
            .args(args)
            .output()
            .wrap_err_with(|| format!("failed to spawn git {:?}", args))?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let lock_error = is_git_index_lock_error(&stderr) || is_git_index_lock_error(&stdout);
        if lock_error && attempt < 5 {
            attempt += 1;
            thread::sleep(Duration::from_millis(50 * u64::from(attempt)));
            continue;
        }

        return Err(eyre!(
            "git {:?} failed in {}: {}{}",
            args,
            project_root.display(),
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(" | stdout: {}", stdout.trim())
            }
        ));
    }
}

fn with_repo_git_lock<T>(project_root: &Path, op: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock = repo_git_lock(project_root);
    let _guard = lock
        .lock()
        .map_err(|_| eyre!("workspace git lock poisoned for {}", project_root.display()))?;
    op()
}

fn repo_git_lock(project_root: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let key = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = locks.lock().expect("workspace git lock map poisoned");
    guard
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn is_git_index_lock_error(output: &str) -> bool {
    output.contains(".git/index.lock")
        || (output.contains("index.lock") && output.contains("Unable to create"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_policy::{WorkspacePolicy, write_workspace_policy};

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
        assert!(
            report.enforced_failures[0]
                .error
                .contains("parse workspace policy failed")
        );
    }

    #[test]
    fn should_report_validation_failures_at_turn_end() {
        let temp = tempfile::tempdir().unwrap();
        let slides_root = temp.path().join("slides").join("deck-a");
        std::fs::create_dir_all(&slides_root).unwrap();
        std::fs::write(slides_root.join("script.js"), "module.exports = [];\n").unwrap();

        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.validation.on_turn_end = vec!["file_exists:output/*.pptx".into()];
        write_workspace_policy(&slides_root, &policy).unwrap();

        let report = snapshot_workspace_turn(temp.path(), "apply user request").unwrap();

        assert_eq!(report.committed, vec!["slides/deck-a"]);
        assert_eq!(report.validation_failures.len(), 1);
        assert_eq!(report.validation_failures[0].repo_label, "slides/deck-a");
        assert_eq!(
            report.validation_failures[0].phase,
            WorkspaceValidationPhase::TurnEnd
        );
        assert_eq!(
            report.validation_failures[0].check,
            "file_exists:output/*.pptx"
        );
        assert!(
            report.validation_failures[0]
                .reason
                .contains("no files match")
        );
    }

    #[test]
    fn should_pass_validation_when_artifact_exists() {
        let temp = tempfile::tempdir().unwrap();
        let slides_root = temp.path().join("slides").join("deck-b");
        std::fs::create_dir_all(&slides_root).unwrap();
        std::fs::write(slides_root.join("script.js"), "module.exports = [];\n").unwrap();

        std::fs::create_dir_all(slides_root.join("output")).unwrap();
        std::fs::write(slides_root.join("output/deck.pptx"), b"PK").unwrap();

        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.validation.on_turn_end = vec!["file_exists:output/*.pptx".into()];
        policy.validation.on_completion = Vec::new();
        write_workspace_policy(&slides_root, &policy).unwrap();

        let report = snapshot_workspace_turn(temp.path(), "apply user request").unwrap();

        assert_eq!(report.committed, vec!["slides/deck-b"]);
        assert!(report.validation_failures.is_empty());
    }

    #[test]
    fn inspection_uses_named_artifact_bindings_for_validator_specs() {
        let temp = tempfile::tempdir().unwrap();
        let slides_root = temp.path().join("slides").join("deck-bundle");
        std::fs::create_dir_all(slides_root.join("output").join("imgs")).unwrap();
        std::fs::write(slides_root.join("script.js"), "module.exports = [];\n").unwrap();
        std::fs::write(slides_root.join("memory.md"), "# memory\n").unwrap();
        std::fs::write(slides_root.join("changelog.md"), "# changelog\n").unwrap();
        std::fs::write(slides_root.join("output/deck.pptx"), b"PK").unwrap();
        std::fs::write(
            slides_root.join("output/imgs/manifest.json"),
            "{\"slides\":[]}\n",
        )
        .unwrap();
        std::fs::write(slides_root.join("output/imgs/slide-01.png"), b"png").unwrap();

        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.validation.on_turn_end = vec!["file_exists:$deck".into()];
        policy.validation.on_completion = vec!["file_exists:$manifest".into()];
        write_workspace_policy(&slides_root, &policy).unwrap();
        initialize_and_commit(
            &slides_root,
            WorkspaceProjectKind::Slides,
            "Initialize slides workspace",
        )
        .unwrap();

        let statuses = inspect_workspace_contracts(temp.path()).unwrap();
        let status = &statuses[0];

        assert_eq!(status.repo_label, "slides/deck-bundle");
        assert!(status.ready);
        assert_eq!(status.turn_end_checks.len(), 1);
        assert!(status.turn_end_checks[0].passed);
        assert_eq!(status.turn_end_checks[0].spec, "file_exists:$deck");
        assert_eq!(status.completion_checks.len(), 1);
        assert!(status.completion_checks[0].passed);
        assert_eq!(status.completion_checks[0].spec, "file_exists:$manifest");
    }

    #[test]
    fn should_skip_validation_when_no_policy() {
        let temp = tempfile::tempdir().unwrap();
        let slides_root = temp.path().join("slides").join("deck-c");
        std::fs::create_dir_all(&slides_root).unwrap();
        std::fs::write(slides_root.join("script.js"), "module.exports = [];\n").unwrap();

        let report = snapshot_workspace_turn(temp.path(), "apply user request").unwrap();
        assert!(report.validation_failures.is_empty());
    }

    #[test]
    fn should_run_completion_validation_when_artifacts_exist() {
        let temp = tempfile::tempdir().unwrap();
        let slides_root = temp.path().join("slides").join("deck-d");
        std::fs::create_dir_all(slides_root.join("output").join("imgs")).unwrap();
        std::fs::write(slides_root.join("script.js"), "module.exports = [];\n").unwrap();
        std::fs::write(slides_root.join("memory.md"), "# memory\n").unwrap();
        std::fs::write(slides_root.join("changelog.md"), "# changelog\n").unwrap();
        std::fs::write(slides_root.join("output/deck.pptx"), b"PK").unwrap();

        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.validation.on_completion = vec![
            "file_exists:output/*.pptx".into(),
            "file_exists:output/**/manifest.json".into(),
        ];
        write_workspace_policy(&slides_root, &policy).unwrap();

        let report = snapshot_workspace_turn(temp.path(), "apply user request").unwrap();

        assert_eq!(report.validation_failures.len(), 1);
        assert_eq!(
            report.validation_failures[0].phase,
            WorkspaceValidationPhase::Completion
        );
        assert_eq!(
            report.validation_failures[0].check,
            "file_exists:output/**/manifest.json"
        );
    }

    #[test]
    fn inspects_workspace_contract_status() {
        let temp = tempfile::tempdir().unwrap();
        let slides_root = temp.path().join("slides").join("deck-e");
        std::fs::create_dir_all(slides_root.join("output").join("imgs")).unwrap();
        std::fs::write(slides_root.join("script.js"), "module.exports = [];\n").unwrap();
        std::fs::write(slides_root.join("memory.md"), "# memory\n").unwrap();
        std::fs::write(slides_root.join("changelog.md"), "# changelog\n").unwrap();
        std::fs::write(slides_root.join("output/deck.pptx"), b"PK").unwrap();
        std::fs::write(
            slides_root.join("output/imgs/manifest.json"),
            "{\"slides\":[]}\n",
        )
        .unwrap();
        std::fs::write(slides_root.join("output/imgs/slide-01.png"), b"png").unwrap();
        write_workspace_policy(
            &slides_root,
            &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
        )
        .unwrap();
        initialize_and_commit(
            &slides_root,
            WorkspaceProjectKind::Slides,
            "Initialize slides workspace",
        )
        .unwrap();

        let statuses = inspect_workspace_contracts(temp.path()).unwrap();
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.repo_label, "slides/deck-e");
        assert!(status.policy_managed);
        assert!(status.ready);
        assert!(status.revision.is_some());
        assert!(!status.dirty);
        assert_eq!(status.artifacts.len(), 3);
        assert!(status.artifacts.iter().all(|artifact| artifact.present));
        assert!(status.turn_end_checks.iter().all(|check| check.passed));
        assert!(status.completion_checks.iter().all(|check| check.passed));
    }

    #[test]
    fn inspection_uses_shared_validator_semantics_for_file_size_checks() {
        let temp = tempfile::tempdir().unwrap();
        let slides_root = temp.path().join("slides").join("deck-f");
        std::fs::create_dir_all(&slides_root).unwrap();
        std::fs::write(slides_root.join("script.js"), "module.exports = [];\n").unwrap();
        std::fs::create_dir_all(slides_root.join("output")).unwrap();
        std::fs::write(slides_root.join("output/deck.pptx"), b"x").unwrap();

        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.validation.on_turn_end = vec!["file_size_min:output/deck.pptx:1024".into()];
        policy.validation.on_completion = Vec::new();
        write_workspace_policy(&slides_root, &policy).unwrap();

        let statuses = inspect_workspace_contracts(temp.path()).unwrap();
        let status = &statuses[0];

        assert_eq!(status.repo_label, "slides/deck-f");
        assert_eq!(status.turn_end_checks.len(), 1);
        assert!(!status.turn_end_checks[0].passed);
        let reason = status.turn_end_checks[0]
            .reason
            .as_deref()
            .expect("inspection reason");
        assert!(reason.contains("output/deck.pptx is 1 bytes, minimum is 1024"));
    }

    #[test]
    fn detects_git_index_lock_errors() {
        assert!(is_git_index_lock_error(
            "fatal: Unable to create 'C:/tmp/.git/index.lock': File exists."
        ));
        assert!(!is_git_index_lock_error("fatal: not a git repository"));
    }
}
