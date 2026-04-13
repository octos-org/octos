use std::collections::HashMap;
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
    pub spawn_tasks: HashMap<String, SpawnTaskPolicy>,
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

/// Behaviour policy for a spawn_only background task.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnTaskPolicy {
    /// Checks to verify the task did what was requested (scope, output validity).
    #[serde(default)]
    pub on_verify: Vec<String>,
    /// Actions to run on successful completion + verify pass (send file, cleanup).
    #[serde(default)]
    pub on_complete: Vec<String>,
    /// Actions to run on failure or verify failure (cleanup, notify).
    #[serde(default)]
    pub on_failure: Vec<String>,
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

impl WorkspacePolicyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Slides => "slides",
            Self::Sites => "sites",
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
                validation: ValidationPolicy::default(),
                spawn_tasks: HashMap::new(),
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
                spawn_tasks: HashMap::new(),
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

    #[test]
    fn should_parse_policy_with_spawn_tasks() {
        let toml_str = r#"
[workspace]
kind = "slides"

[version_control]
provider = "git"
auto_init = true
trigger = "turn_end"
fail_on_error = true

[tracking]
ignore = ["output/**"]

[spawn_tasks.tts]
on_verify = ["file_exists:output/*.mp3"]
on_complete = ["send_file:output/*.mp3", "cleanup:temp/tts_*"]
on_failure = ["cleanup:temp/tts_*", "notify_user:TTS generation failed"]

[spawn_tasks.slides]
on_verify = ["scope_check:modified_slides"]
on_complete = ["send_file:output/*.pptx"]
on_failure = ["cleanup:temp/slides_*"]
"#;

        let policy: WorkspacePolicy = toml::from_str(toml_str).unwrap();
        assert_eq!(policy.spawn_tasks.len(), 2);

        let tts = &policy.spawn_tasks["tts"];
        assert_eq!(tts.on_verify, vec!["file_exists:output/*.mp3"]);
        assert_eq!(tts.on_complete.len(), 2);
        assert_eq!(tts.on_failure.len(), 2);

        let slides = &policy.spawn_tasks["slides"];
        assert_eq!(slides.on_verify, vec!["scope_check:modified_slides"]);
    }

    #[test]
    fn should_parse_policy_with_validation() {
        let toml_str = r#"
[workspace]
kind = "sites"

[version_control]
provider = "git"
auto_init = true
trigger = "turn_end"
fail_on_error = true

[tracking]
ignore = []

[validation]
on_turn_end = ["file_exists:index.html", "build_check"]
on_source_change = ["preview_render"]
on_completion = ["playwright:homepage_loads"]
"#;

        let policy: WorkspacePolicy = toml::from_str(toml_str).unwrap();
        assert_eq!(policy.validation.on_turn_end.len(), 2);
        assert_eq!(policy.validation.on_source_change, vec!["preview_render"]);
        assert_eq!(
            policy.validation.on_completion,
            vec!["playwright:homepage_loads"]
        );
    }

    #[test]
    fn should_default_new_sections_when_absent() {
        let toml_str = r#"
[workspace]
kind = "slides"

[version_control]
provider = "git"
auto_init = true
trigger = "turn_end"
fail_on_error = true

[tracking]
ignore = []
"#;

        let policy: WorkspacePolicy = toml::from_str(toml_str).unwrap();
        assert!(policy.validation.on_turn_end.is_empty());
        assert!(policy.validation.on_source_change.is_empty());
        assert!(policy.validation.on_completion.is_empty());
        assert!(policy.spawn_tasks.is_empty());
    }

    #[test]
    fn should_roundtrip_policy_with_spawn_tasks() {
        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.spawn_tasks.insert(
            "tts".into(),
            SpawnTaskPolicy {
                on_verify: vec!["file_exists:output/*.mp3".into()],
                on_complete: vec!["send_file:output/*.mp3".into()],
                on_failure: vec!["cleanup:temp/tts_*".into()],
            },
        );
        policy.validation = ValidationPolicy {
            on_turn_end: vec!["file_exists:output/*.pptx".into()],
            on_source_change: vec![],
            on_completion: vec![],
        };

        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &policy).unwrap();
        let roundtrip = read_workspace_policy(temp.path()).unwrap().unwrap();
        assert_eq!(roundtrip, policy);
    }
}
