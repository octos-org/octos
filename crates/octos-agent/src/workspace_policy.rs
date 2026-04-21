use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::abi_schema::{
    WORKSPACE_POLICY_SCHEMA_VERSION, check_supported, default_workspace_policy_schema_version,
};
use crate::workspace_git::WorkspaceProjectKind;

pub const WORKSPACE_POLICY_FILE: &str = ".octos-workspace.toml";

/// Harness-facing workspace policy.
///
/// `schema_version` is the durable ABI version; see
/// `docs/OCTOS_HARNESS_ABI_VERSIONING.md` for the stable and experimental
/// fields per version. Older policy files that omit the field are accepted
/// as v1 on deserialization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePolicy {
    /// Durable ABI schema version for this policy. Defaults to
    /// [`WORKSPACE_POLICY_SCHEMA_VERSION`] when absent so pre-versioned
    /// policies continue to load.
    #[serde(default = "default_workspace_policy_schema_version")]
    pub schema_version: u32,
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
    /// Typed declarative validators (M4.3). Runs via `ValidatorRunner`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validators: Vec<Validator>,
}

/// Typed declarative validator spec.
///
/// Each validator is identified by a stable `id`, produces a typed
/// [`crate::validators::ValidatorOutcome`], and may be `required` (a failure
/// blocks terminal success) or optional (a failure produces a warning only).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validator {
    /// Stable identifier, unique within the validator list.
    pub id: String,
    /// Required validators block terminal success when they fail.
    #[serde(default = "default_required")]
    pub required: bool,
    /// Optional per-validator timeout in milliseconds. Applies to command and
    /// tool validators. File-existence validators ignore the timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Which lifecycle phase this validator runs in. Defaults to completion.
    #[serde(default, skip_serializing_if = "is_default_phase")]
    pub phase: ValidatorPhaseKind,
    #[serde(flatten)]
    pub spec: ValidatorSpec,
}

fn default_required() -> bool {
    true
}

fn is_default_phase(phase: &ValidatorPhaseKind) -> bool {
    *phase == ValidatorPhaseKind::default()
}

/// Lifecycle phase a validator runs in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorPhaseKind {
    /// Runs on every turn end (cheap checks).
    TurnEnd,
    /// Runs on completion / publish (expensive checks).
    #[default]
    Completion,
}

/// The typed body of a [`Validator`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValidatorSpec {
    /// Run a subprocess command. Dispatched via the shell-safety layer and
    /// existing `BLOCKED_ENV_VARS` sanitization. No direct `Command::new("sh")`
    /// bypass.
    Command {
        cmd: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
    /// Invoke a registered agent tool. Outcome status follows the tool's
    /// `ToolResult.success`.
    ToolCall {
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// Assert that a file exists (and optionally meets a minimum byte count).
    FileExists {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_bytes: Option<u64>,
    },
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
    /// Legacy completion hook retained for compatibility. Prefer `on_deliver`
    /// for explicit handoff/delivery actions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_complete: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_deliver: Vec<String>,
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

    pub fn delivery_actions(&self) -> &[String] {
        if self.on_deliver.is_empty() {
            &self.on_complete
        } else {
            &self.on_deliver
        }
    }
}

impl WorkspacePolicy {
    pub fn for_kind(kind: WorkspaceProjectKind) -> Self {
        match kind {
            WorkspaceProjectKind::Slides => Self {
                schema_version: WORKSPACE_POLICY_SCHEMA_VERSION,
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
                        "file_exists:output/deck.pptx".into(),
                        "file_exists:output/**/slide-*.png".into(),
                    ],
                    validators: Vec::new(),
                },
                artifacts: WorkspaceArtifactsPolicy {
                    entries: BTreeMap::from([
                        ("primary".into(), "output/deck.pptx".into()),
                        ("deck".into(), "output/deck.pptx".into()),
                        ("previews".into(), "output/**/slide-*.png".into()),
                    ]),
                },
                spawn_tasks: BTreeMap::new(),
            },
            WorkspaceProjectKind::Sites => Self {
                schema_version: WORKSPACE_POLICY_SCHEMA_VERSION,
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
            on_deliver: vec![],
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
            on_deliver: vec![],
            on_failure: vec!["notify_user:Podcast generation failed".into()],
        };

        let mut spawn_tasks = BTreeMap::new();
        spawn_tasks.insert("fm_tts".into(), tts_contract.clone());
        spawn_tasks.insert("voice_synthesize".into(), tts_contract);
        spawn_tasks.insert("podcast_generate".into(), podcast_contract);

        Self {
            schema_version: WORKSPACE_POLICY_SCHEMA_VERSION,
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

    pub fn for_site_build_output(build_output_dir: &str) -> Self {
        let mut policy = Self::for_kind(WorkspaceProjectKind::Sites);
        policy.validation = ValidationPolicy {
            on_turn_end: vec![
                "file_exists:mofa-site-session.json".into(),
                "file_exists:site-plan.json".into(),
                "file_exists:optimized-prompt.md".into(),
            ],
            on_source_change: Vec::new(),
            on_completion: vec![format!("file_exists:{build_output_dir}/index.html")],
            validators: Vec::new(),
        };
        policy.artifacts = WorkspaceArtifactsPolicy {
            entries: BTreeMap::from([
                ("primary".into(), format!("{build_output_dir}/index.html")),
                (
                    "entrypoint".into(),
                    format!("{build_output_dir}/index.html"),
                ),
            ]),
        };
        policy
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
    check_supported(
        "WorkspacePolicy",
        policy.schema_version,
        WORKSPACE_POLICY_SCHEMA_VERSION,
    )
    .wrap_err_with(|| format!("incompatible workspace policy: {}", path.display()))?;
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
                "file_exists:output/deck.pptx",
                "file_exists:output/**/slide-*.png",
            ]
        );
        assert_eq!(
            policy.artifacts.entries.get("primary").map(String::as_str),
            Some("output/deck.pptx")
        );
        assert_eq!(
            policy.artifacts.entries.get("deck").map(String::as_str),
            Some("output/deck.pptx")
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
    fn site_build_output_policy_requires_entrypoint() {
        let policy = WorkspacePolicy::for_site_build_output("dist");
        assert_eq!(
            policy.validation.on_turn_end,
            vec![
                "file_exists:mofa-site-session.json",
                "file_exists:site-plan.json",
                "file_exists:optimized-prompt.md",
            ]
        );
        assert_eq!(
            policy.validation.on_completion,
            vec!["file_exists:dist/index.html"]
        );
        assert_eq!(
            policy.artifacts.entries.get("primary").map(String::as_str),
            Some("dist/index.html")
        );
        assert_eq!(
            policy
                .artifacts
                .entries
                .get("entrypoint")
                .map(String::as_str),
            Some("dist/index.html")
        );
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
        assert!(task.on_deliver.is_empty());

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
        assert!(podcast_task.on_deliver.is_empty());
    }

    #[test]
    fn spawn_task_artifact_sources_prefer_multi_artifact_list() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("legacy".into()),
            artifacts: vec!["report".into(), "audio".into()],
            on_verify: Vec::new(),
            on_complete: Vec::new(),
            on_deliver: Vec::new(),
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
            on_deliver: Vec::new(),
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
            on_deliver: Vec::new(),
            on_failure: Vec::new(),
        };

        let rendered = toml::to_string_pretty(&task).unwrap();
        assert!(!rendered.contains("artifacts = []"));
        let roundtrip: WorkspaceSpawnTaskPolicy = toml::from_str(&rendered).unwrap();
        assert_eq!(roundtrip.artifact_sources(), vec!["primary_audio"]);
    }

    #[test]
    fn spawn_task_delivery_actions_prefer_explicit_delivery_list() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec!["notify_user:legacy".into()],
            on_deliver: vec!["notify_user:deliver".into()],
            on_failure: Vec::new(),
        };

        assert_eq!(
            task.delivery_actions(),
            &["notify_user:deliver".to_string()]
        );
    }

    #[test]
    fn spawn_task_delivery_actions_fall_back_to_legacy_completion_list() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec!["notify_user:legacy".into()],
            on_deliver: Vec::new(),
            on_failure: Vec::new(),
        };

        assert_eq!(task.delivery_actions(), &["notify_user:legacy".to_string()]);
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

    #[test]
    fn should_stamp_current_schema_version_when_building_for_kind() {
        let slides = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        let sites = WorkspacePolicy::for_kind(WorkspaceProjectKind::Sites);
        let session = WorkspacePolicy::for_session();
        assert_eq!(slides.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION);
        assert_eq!(sites.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION);
        assert_eq!(session.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION);
    }

    #[test]
    fn should_default_missing_schema_version_to_v1_when_loading_legacy_toml() {
        // A TOML emitted before M4.6 — no `schema_version` line.
        let legacy = r#"
[workspace]
kind = "slides"

[version_control]
provider = "git"
auto_init = true
trigger = "turn_end"
fail_on_error = true

[tracking]
ignore = ["output/**"]
"#;
        let parsed: WorkspacePolicy = toml::from_str(legacy).expect("legacy policy should parse");
        assert_eq!(parsed.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION);
        assert_eq!(parsed.workspace.kind, WorkspacePolicyKind::Slides);
    }

    #[test]
    fn should_reject_future_schema_version_with_actionable_error() {
        // A TOML that claims a version the harness cannot understand.
        let future = format!(
            r#"
schema_version = {}

[workspace]
kind = "slides"

[version_control]
provider = "git"
auto_init = true
trigger = "turn_end"
fail_on_error = true

[tracking]
ignore = []
"#,
            WORKSPACE_POLICY_SCHEMA_VERSION + 99
        );
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(WORKSPACE_POLICY_FILE), future).unwrap();

        let err = read_workspace_policy(temp.path()).expect_err("future version should fail");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("schema_version"));
        assert!(rendered.contains("upgrade octos"));
    }

    #[test]
    fn should_roundtrip_typed_validators_through_toml() {
        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.validation.validators = vec![
            Validator {
                id: "cmd".into(),
                required: true,
                timeout_ms: Some(3000),
                phase: ValidatorPhaseKind::Completion,
                spec: ValidatorSpec::Command {
                    cmd: "echo".into(),
                    args: vec!["hello".into()],
                },
            },
            Validator {
                id: "file".into(),
                required: false,
                timeout_ms: None,
                phase: ValidatorPhaseKind::TurnEnd,
                spec: ValidatorSpec::FileExists {
                    path: "out.txt".into(),
                    min_bytes: Some(128),
                },
            },
            Validator {
                id: "tool".into(),
                required: true,
                timeout_ms: Some(5000),
                phase: ValidatorPhaseKind::Completion,
                spec: ValidatorSpec::ToolCall {
                    tool: "custom_tool".into(),
                    args: serde_json::json!({"mode": "strict"}),
                },
            },
        ];
        let rendered = toml::to_string_pretty(&policy).unwrap();
        assert!(rendered.contains("[[validation.validators]]"));
        assert!(rendered.contains("kind = \"command\""));
        assert!(rendered.contains("kind = \"file_exists\""));
        assert!(rendered.contains("kind = \"tool_call\""));
        let parsed: WorkspacePolicy = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn validator_defaults_to_required_and_completion_phase() {
        let toml = r#"
            id = "x"
            kind = "file_exists"
            path = "output.txt"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        assert_eq!(parsed.id, "x");
        assert!(parsed.required, "required defaults to true");
        assert_eq!(parsed.phase, ValidatorPhaseKind::Completion);
        assert!(parsed.timeout_ms.is_none());
    }
}
