use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::workspace_git::WorkspaceProjectKind;

pub const WORKSPACE_POLICY_FILE: &str = ".octos-workspace.toml";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePolicy {
    pub workspace: WorkspacePolicyWorkspace,
    pub version_control: WorkspaceVersionControlPolicy,
    pub tracking: WorkspaceTrackingPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePolicyWorkspace {
    pub kind: WorkspacePolicyKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspacePolicyKind {
    Slides,
    Sites,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceVersionControlPolicy {
    pub provider: WorkspaceVersionControlProvider,
    pub auto_init: bool,
    pub trigger: WorkspaceSnapshotTrigger,
    pub fail_on_error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceVersionControlProvider {
    Git,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSnapshotTrigger {
    TurnEnd,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTrackingPolicy {
    pub ignore: Vec<String>,
}

impl WorkspacePolicy {
    pub fn for_kind(kind: WorkspaceProjectKind) -> Self {
        match kind {
            WorkspaceProjectKind::Slides => Self {
                workspace: WorkspacePolicyWorkspace {
                    kind: WorkspacePolicyKind::Slides,
                },
                version_control: WorkspaceVersionControlPolicy {
                    provider: WorkspaceVersionControlProvider::Git,
                    auto_init: true,
                    trigger: WorkspaceSnapshotTrigger::TurnEnd,
                    fail_on_error: true,
                },
                tracking: WorkspaceTrackingPolicy {
                    ignore: vec![
                        "history/**".into(),
                        "output/**".into(),
                        "skill-output/**".into(),
                        "*.pptx".into(),
                        "*.tmp".into(),
                        ".DS_Store".into(),
                    ],
                },
            },
            WorkspaceProjectKind::Sites => Self {
                workspace: WorkspacePolicyWorkspace {
                    kind: WorkspacePolicyKind::Sites,
                },
                version_control: WorkspaceVersionControlPolicy {
                    provider: WorkspaceVersionControlProvider::Git,
                    auto_init: true,
                    trigger: WorkspaceSnapshotTrigger::TurnEnd,
                    fail_on_error: true,
                },
                tracking: WorkspaceTrackingPolicy {
                    ignore: vec![
                        "node_modules/**".into(),
                        "dist/**".into(),
                        "out/**".into(),
                        "docs/**".into(),
                        "build/**".into(),
                        ".astro/**".into(),
                        ".next/**".into(),
                        ".quarto/**".into(),
                        "*.log".into(),
                        ".DS_Store".into(),
                    ],
                },
            },
        }
    }
}

pub fn workspace_policy_path(project_root: &Path) -> PathBuf {
    project_root.join(WORKSPACE_POLICY_FILE)
}

pub fn read_workspace_policy(project_root: &Path) -> Result<Option<WorkspacePolicy>> {
    let path = workspace_policy_path(project_root);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .wrap_err_with(|| format!("read workspace policy failed: {}", path.display()))?;
    let policy: WorkspacePolicy = toml::from_str(&raw)
        .wrap_err_with(|| format!("parse workspace policy failed: {}", path.display()))?;
    Ok(Some(policy))
}

pub fn write_workspace_policy(project_root: &Path, policy: &WorkspacePolicy) -> Result<()> {
    std::fs::create_dir_all(project_root)
        .wrap_err_with(|| format!("create project dir failed: {}", project_root.display()))?;
    let path = workspace_policy_path(project_root);
    let rendered = toml::to_string_pretty(policy)
        .wrap_err_with(|| format!("serialize workspace policy failed: {}", path.display()))?;
    std::fs::write(&path, rendered)
        .wrap_err_with(|| format!("write workspace policy failed: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_slides_policy() {
        let temp = tempfile::tempdir().unwrap();
        let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);

        write_workspace_policy(temp.path(), &policy).unwrap();

        let path = workspace_policy_path(temp.path());
        assert!(path.is_file());

        let rendered = std::fs::read_to_string(&path).unwrap();
        assert!(rendered.contains("kind = \"slides\""));
        assert!(rendered.contains("provider = \"git\""));
        assert!(rendered.contains("trigger = \"turn_end\""));
        assert!(rendered.contains("\"output/**\""));

        let roundtrip = read_workspace_policy(temp.path()).unwrap().unwrap();
        assert_eq!(roundtrip, policy);
    }

    #[test]
    fn default_site_policy_tracks_build_outputs_as_ignored() {
        let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Sites);
        assert!(policy.tracking.ignore.iter().any(|item| item == "dist/**"));
        assert!(policy.tracking.ignore.iter().any(|item| item == ".next/**"));
    }
}
