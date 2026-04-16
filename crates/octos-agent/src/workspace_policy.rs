use std::collections::BTreeMap;
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
    #[serde(default)]
    pub validation: ValidationPolicy,
    #[serde(default)]
    pub artifacts: WorkspaceArtifactsPolicy,
    #[serde(default)]
    pub spawn_tasks: BTreeMap<String, WorkspaceSpawnTaskPolicy>,
}

/// Tiered validation checks run at different points in the turn lifecycle.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationPolicy {
    /// Tier 1: cheap checks run every turn (< 100ms). e.g. file_exists, build exit code.
    #[serde(default)]
    pub on_turn_end: Vec<String>,
    /// Tier 2: medium checks run when source files change (1-5s). e.g. preview render.
    #[serde(default)]
    pub on_source_change: Vec<String>,
    /// Tier 3: expensive checks run on completion/publish only (10-30s). e.g. Playwright.
    #[serde(default)]
    pub on_completion: Vec<String>,
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
    Session,
}

impl WorkspacePolicyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Slides => "slides",
            Self::Sites => "sites",
            Self::Session => "session",
        }
    }

    pub fn matches_project_kind(self, kind: WorkspaceProjectKind) -> bool {
        self == Self::from(kind)
    }
}

impl From<WorkspaceProjectKind> for WorkspacePolicyKind {
    fn from(value: WorkspaceProjectKind) -> Self {
        match value {
            WorkspaceProjectKind::Slides => Self::Slides,
            WorkspaceProjectKind::Sites => Self::Sites,
        }
    }
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkspaceArtifactsPolicy {
    #[serde(flatten)]
    pub entries: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkspaceSpawnTaskPolicy {
    #[serde(default)]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub on_verify: Vec<String>,
    #[serde(default)]
    pub on_complete: Vec<String>,
    #[serde(default)]
    pub on_failure: Vec<String>,
}

impl WorkspaceSpawnTaskPolicy {
    pub fn artifact_sources(&self) -> Vec<&str> {
        if self.artifacts.is_empty() {
            self.artifact.iter().map(String::as_str).collect()
        } else {
            self.artifacts.iter().map(String::as_str).collect()
        }
    }
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
                validation: ValidationPolicy {
                    on_turn_end: vec![
                        "file_exists:script.js".into(),
                        "file_exists:memory.md".into(),
                        "file_exists:changelog.md".into(),
                    ],
                    on_source_change: Vec::new(),
                    on_completion: vec![
                        "file_exists:output/*.pptx".into(),
                        "file_exists:output/**/manifest.json".into(),
                        "file_exists:output/**/slide-*.png".into(),
                    ],
                },
                artifacts: WorkspaceArtifactsPolicy {
                    entries: BTreeMap::from([
                        ("deck".into(), "output/*.pptx".into()),
                        ("manifest".into(), "output/**/manifest.json".into()),
                        ("previews".into(), "output/**/slide-*.png".into()),
                    ]),
                },
                spawn_tasks: BTreeMap::new(),
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
                validation: ValidationPolicy::default(),
                artifacts: WorkspaceArtifactsPolicy::default(),
                spawn_tasks: BTreeMap::new(),
            },
        }
    }

    pub fn for_session() -> Self {
        let mut artifacts = BTreeMap::new();
        artifacts.insert("primary_audio".into(), "*.mp3".into());
        artifacts.insert("podcast_audio".into(), "**/podcast_full_*.*".into());

        let tts_contract = WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: vec![
                "file_exists:$artifact".into(),
                "file_size_min:$artifact:1024".into(),
            ],
            on_complete: vec![],
            on_failure: vec!["notify_user:TTS generation failed".into()],
        };

        let podcast_contract = WorkspaceSpawnTaskPolicy {
            artifact: Some("podcast_audio".into()),
            artifacts: Vec::new(),
            on_verify: vec![
                "file_exists:$artifact".into(),
                "file_size_min:$artifact:4096".into(),
            ],
            on_complete: vec![],
            on_failure: vec!["notify_user:Podcast generation failed".into()],
        };

        let mut spawn_tasks = BTreeMap::new();
        spawn_tasks.insert("fm_tts".into(), tts_contract.clone());
        spawn_tasks.insert("voice_synthesize".into(), tts_contract);
        spawn_tasks.insert("podcast_generate".into(), podcast_contract);

        Self {
            workspace: WorkspacePolicyWorkspace {
                kind: WorkspacePolicyKind::Session,
            },
            version_control: WorkspaceVersionControlPolicy {
                provider: WorkspaceVersionControlProvider::Git,
                auto_init: false,
                trigger: WorkspaceSnapshotTrigger::TurnEnd,
                fail_on_error: false,
            },
            tracking: WorkspaceTrackingPolicy {
                ignore: vec!["tmp/**".into(), ".DS_Store".into()],
            },
            validation: ValidationPolicy::default(),
            artifacts: WorkspaceArtifactsPolicy { entries: artifacts },
            spawn_tasks,
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

pub fn upgrade_workspace_policy_if_legacy(
    policy: &WorkspacePolicy,
    kind: WorkspaceProjectKind,
) -> Option<WorkspacePolicy> {
    match kind {
        WorkspaceProjectKind::Slides if *policy == legacy_slides_workspace_policy() => {
            Some(WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides))
        }
        WorkspaceProjectKind::Slides | WorkspaceProjectKind::Sites => None,
    }
}

fn legacy_slides_workspace_policy() -> WorkspacePolicy {
    let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
    policy.validation = ValidationPolicy::default();
    policy.artifacts = WorkspaceArtifactsPolicy::default();
    policy.spawn_tasks.clear();
    policy
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
    fn slides_policy_has_default_contract() {
        let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);

        assert_eq!(
            policy.validation.on_turn_end,
            vec![
                "file_exists:script.js",
                "file_exists:memory.md",
                "file_exists:changelog.md",
            ]
        );
        assert_eq!(
            policy.validation.on_completion,
            vec![
                "file_exists:output/*.pptx",
                "file_exists:output/**/manifest.json",
                "file_exists:output/**/slide-*.png",
            ]
        );
        assert_eq!(
            policy.artifacts.entries.get("deck").map(String::as_str),
            Some("output/*.pptx")
        );
        assert_eq!(
            policy.artifacts.entries.get("manifest").map(String::as_str),
            Some("output/**/manifest.json")
        );
        assert_eq!(
            policy.artifacts.entries.get("previews").map(String::as_str),
            Some("output/**/slide-*.png")
        );
    }

    #[test]
    fn default_site_policy_tracks_build_outputs_as_ignored() {
        let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Sites);
        assert!(policy.tracking.ignore.iter().any(|item| item == "dist/**"));
        assert!(policy.tracking.ignore.iter().any(|item| item == ".next/**"));
    }

    #[test]
    fn session_policy_declares_tts_contract() {
        let policy = WorkspacePolicy::for_session();
        assert_eq!(policy.workspace.kind, WorkspacePolicyKind::Session);
        assert_eq!(
            policy
                .artifacts
                .entries
                .get("primary_audio")
                .map(String::as_str),
            Some("*.mp3")
        );
        let task = policy.spawn_tasks.get("fm_tts").expect("fm_tts contract");
        assert_eq!(task.artifact.as_deref(), Some("primary_audio"));
        assert!(task.artifacts.is_empty());
        assert!(task.on_complete.is_empty());

        assert_eq!(
            policy
                .artifacts
                .entries
                .get("podcast_audio")
                .map(String::as_str),
            Some("**/podcast_full_*.*")
        );
        let podcast_task = policy
            .spawn_tasks
            .get("podcast_generate")
            .expect("podcast_generate contract");
        assert_eq!(podcast_task.artifact.as_deref(), Some("podcast_audio"));
        assert!(podcast_task.artifacts.is_empty());
        assert!(
            podcast_task
                .on_verify
                .iter()
                .any(|action| action == "file_size_min:$artifact:4096")
        );
    }

    #[test]
    fn spawn_task_artifact_sources_prefer_multi_artifact_list() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("legacy".into()),
            artifacts: vec!["report".into(), "audio".into()],
            on_verify: Vec::new(),
            on_complete: Vec::new(),
            on_failure: Vec::new(),
        };

        assert_eq!(task.artifact_sources(), vec!["report", "audio"]);
    }

    #[test]
    fn spawn_task_artifact_sources_fall_back_to_single_artifact() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: Vec::new(),
            on_failure: Vec::new(),
        };

        assert_eq!(task.artifact_sources(), vec!["primary_audio"]);
    }

    #[test]
    fn spawn_task_artifact_sources_roundtrip_omits_empty_list() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: vec!["file_exists:$artifact".into()],
            on_complete: Vec::new(),
            on_failure: Vec::new(),
        };

        let rendered = toml::to_string_pretty(&task).unwrap();
        assert!(!rendered.contains("artifacts = []"));
        let roundtrip: WorkspaceSpawnTaskPolicy = toml::from_str(&rendered).unwrap();
        assert_eq!(roundtrip.artifact_sources(), vec!["primary_audio"]);
    }

    #[test]
    fn upgrades_legacy_slides_policy_to_default_contract() {
        let legacy = legacy_slides_workspace_policy();
        let upgraded = upgrade_workspace_policy_if_legacy(&legacy, WorkspaceProjectKind::Slides)
            .expect("legacy slides policy should upgrade");

        assert_eq!(
            upgraded,
            WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides)
        );
    }

    #[test]
    fn does_not_upgrade_non_legacy_slides_policy() {
        let current = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        assert!(
            upgrade_workspace_policy_if_legacy(&current, WorkspaceProjectKind::Slides).is_none()
        );
    }
}
