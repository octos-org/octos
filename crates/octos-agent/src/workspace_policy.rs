use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::abi_schema::{
    COMPACTION_POLICY_SCHEMA_VERSION, WORKSPACE_POLICY_SCHEMA_VERSION, check_supported,
    default_compaction_policy_schema_version, default_workspace_policy_schema_version,
};
use crate::workspace_git::WorkspaceProjectKind;

pub const WORKSPACE_POLICY_FILE: &str = ".octos-workspace.toml";

/// Harness-facing workspace policy.
///
/// `schema_version` is the durable ABI version; see
/// `docs/OCTOS_HARNESS_ABI_VERSIONING.md` for the stable and experimental
/// fields per version. Older policy files that omit the field are accepted
/// as v1 on deserialization.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// Declarative compaction contract (harness M6.3). Absent = legacy extractive
    /// behaviour with no preflight or typed placeholders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionPolicy>,
}

/// Harness-facing compaction contract (M6.3).
///
/// Declares the shape of compaction for a workspace: how many tokens to aim
/// for, which declared artifacts must survive the pass, when to pre-emptively
/// compact before the first LLM call, and how aggressively to prune stale
/// tool outputs. When absent, the runtime falls back to the legacy extractive
/// path and behaves exactly as before M6.3.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionPolicy {
    /// Durable ABI schema version. See
    /// [`COMPACTION_POLICY_SCHEMA_VERSION`]. Missing in legacy files;
    /// defaulted to the current version via
    /// [`default_compaction_policy_schema_version`].
    #[serde(default = "default_compaction_policy_schema_version")]
    pub schema_version: u32,
    /// Target token budget for the compacted conversation after a pass.
    pub token_budget: u32,
    /// Artifact names (keys in `artifacts`) whose declared patterns MUST be
    /// referenced at least once in the compacted message stream. Failure here
    /// trips the validator rail and blocks terminal success.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserved_artifacts: Vec<String>,
    /// Free-form substrings that must survive compaction (e.g. a workspace
    /// invariant flag string). Matched verbatim against message content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserved_invariants: Vec<String>,
    /// Summarizer flavour to use for the compaction pass. Defaults to the
    /// extractive variant until M6.4 wires the LLM-iterative implementation.
    #[serde(default)]
    pub summarizer: CompactionSummarizerKind,
    /// Trigger preflight compaction before the first LLM call when the
    /// conversation already exceeds this token count. `None` disables
    /// preflight entirely (post-call compaction still runs on overflow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight_threshold: Option<u32>,
    /// Replace tool results older than N user-turn boundaries with a typed
    /// `ToolResultPlaceholder`. `None` keeps tool results intact until the
    /// usual token-budget path kicks in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune_tool_results_after_turns: Option<u32>,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
            token_budget: 8_000,
            preserved_artifacts: Vec::new(),
            preserved_invariants: Vec::new(),
            summarizer: CompactionSummarizerKind::default(),
            preflight_threshold: None,
            prune_tool_results_after_turns: None,
        }
    }
}

/// Summarizer strategy declared in a [`CompactionPolicy`]. The runtime maps
/// this to an implementation of [`crate::summarizer::Summarizer`] at wire
/// time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionSummarizerKind {
    /// Deterministic extractive summarizer (preserves legacy behaviour).
    #[default]
    Extractive,
    /// LLM-iterative summarizer. Lands in M6.4; the extractive summarizer is
    /// used as a fallback in the current runtime.
    LlmIterative,
}

/// Tiered validation checks run at different points in the turn lifecycle.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// HTTP probe — call URL (with `${args.<path>}` template interpolation
    /// against the spawn task's input args), assert the response is the
    /// expected status code, optionally assert a substring is present in the
    /// response body. Default timeout 5s (overridden by
    /// [`Validator::timeout_ms`]).
    HttpProbe {
        url_template: String,
        #[serde(default = "default_http_probe_status")]
        expected_status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_contains: Option<String>,
    },
    /// Specialization of `HttpProbe` for the common case of asserting
    /// ominix-api has registered a custom voice. Calls
    /// `GET ${OMINIX_API_URL:-http://127.0.0.1:8081}/v1/voices` and
    /// asserts the response's `voices[].name` array contains the
    /// interpolated `name_arg` value. Surfaces the available list in the
    /// failure message so the LLM can react in one round.
    OminixVoiceExists {
        /// Argument key in the spawn task's input args (e.g. `name`) that
        /// holds the voice name to look up.
        name_arg: String,
    },
    /// Assert that at least one file matching `glob` has decoded audio with
    /// `non_silent_samples / total_samples >= min_ratio`. WAV is supported
    /// natively. MP3 support requires the `audio_mp3` feature flag.
    AudioNonSilent {
        glob: String,
        #[serde(default = "default_non_silent_ratio")]
        min_ratio: f32,
    },
    /// Assert each file matching `glob` has the magic-byte prefix for the
    /// declared `format`. Catches "tool wrote 0 bytes" or "tool wrote an
    /// HTML error page in place of an MP3".
    ///
    /// The format field is named `format` rather than `kind` to avoid
    /// colliding with serde's `kind` discriminator tag.
    MagicBytes { glob: String, format: MagicByteKind },
}

/// File-format signature used by [`ValidatorSpec::MagicBytes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MagicByteKind {
    Mp3,
    Wav,
    Png,
    Jpeg,
    Pdf,
    Mp4,
    WebM,
}

impl MagicByteKind {
    /// Return the alternative magic-byte prefixes for this file format.
    /// A file matches if any prefix is present at the start of the byte
    /// stream.
    pub fn prefixes(self) -> &'static [&'static [u8]] {
        match self {
            // MP3 with ID3v2 tag, or a raw MPEG frame sync (0xFF Fx/Ex/Dx).
            Self::Mp3 => &[
                b"ID3",
                &[0xFF, 0xFB],
                &[0xFF, 0xFA],
                &[0xFF, 0xF3],
                &[0xFF, 0xF2],
                &[0xFF, 0xE3],
                &[0xFF, 0xE2],
            ],
            Self::Wav => &[b"RIFF"],
            Self::Png => &[&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]],
            Self::Jpeg => &[&[0xFF, 0xD8, 0xFF]],
            Self::Pdf => &[b"%PDF-"],
            // MP4: 4-byte size prefix followed by 'ftyp'. Most MP4s also
            // start with 'ftyp' offset by 4 bytes, but checking the brand
            // directly is simpler — see `magic_bytes_match`.
            Self::Mp4 => &[b"ftyp"],
            Self::WebM => &[&[0x1A, 0x45, 0xDF, 0xA3]],
        }
    }

    /// Does `data` start with one of the prefixes for this format?
    ///
    /// For MP4, the `ftyp` marker lives at offset 4 (after the box-size
    /// prefix), so the check is byte-position aware. For other formats the
    /// prefix is at the beginning.
    pub fn matches(self, data: &[u8]) -> bool {
        if self == Self::Mp4 {
            // MP4: bytes 4..8 must be 'ftyp'.
            return data.len() >= 8 && &data[4..8] == b"ftyp";
        }
        let prefixes = self.prefixes();
        prefixes.iter().any(|p| data.starts_with(p))
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Pdf => "pdf",
            Self::Mp4 => "mp4",
            Self::WebM => "webm",
        }
    }
}

fn default_http_probe_status() -> u16 {
    200
}

fn default_non_silent_ratio() -> f32 {
    0.3
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
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
    /// Per-spawn-task typed validators run at the completion gate, in
    /// addition to the workspace-wide `[validation].validators`. Each entry
    /// is auto-tagged as required+completion phase; pass an explicit
    /// `Validator` struct (with `id`, `required`, etc.) for finer control.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_completion: Vec<SpawnTaskValidatorSpec>,
}

/// TOML-friendly wrapper for the per-spawn-task `on_completion` validator
/// list. Accepts either:
///
/// * A bare `ValidatorSpec` table (no `id`/`required`/`phase`) — auto-tagged
///   as required + completion phase + a synthetic `id` derived from the
///   spawn task name and validator index.
/// * A full `Validator` table with `id`, `required`, `timeout_ms`, etc.
///
/// Both forms surface to the runner as a [`Validator`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SpawnTaskValidatorSpec {
    /// Full Validator struct with `id`, `required`, etc.
    Full(Validator),
    /// Bare spec table — id, required, and phase are auto-filled by
    /// [`SpawnTaskValidatorSpec::into_validator`].
    Bare(ValidatorSpec),
}

impl SpawnTaskValidatorSpec {
    /// Lower this entry into a fully-formed `Validator` using `task_name`
    /// and `index` to synthesize a stable id when only a bare spec was
    /// provided.
    pub fn into_validator(self, task_name: &str, index: usize) -> Validator {
        match self {
            Self::Full(validator) => validator,
            Self::Bare(spec) => Validator {
                id: format!("{task_name}.on_completion[{index}]"),
                required: true,
                timeout_ms: None,
                phase: ValidatorPhaseKind::Completion,
                spec,
            },
        }
    }
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
                compaction: None,
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
                compaction: None,
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
            on_completion: Vec::new(),
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
            // Catch the two user-visible failure modes from the silent-MP3
            // bug class: tool wrote zero bytes / an HTML error page in
            // place of audio (MagicBytes), or tool generated a valid MP3
            // header but a silent decoded stream (AudioNonSilent).
            on_completion: vec![
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                    glob: "skill-output/mofa-podcast/*.mp3".into(),
                    format: MagicByteKind::Mp3,
                }),
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::AudioNonSilent {
                    glob: "skill-output/mofa-podcast/*.mp3".into(),
                    min_ratio: default_non_silent_ratio(),
                }),
            ],
        };

        // Voice synthesis (LLM-driven TTS): assert the decoded audio is not
        // silent. Catches the "render produced empty audio" failure path.
        let voice_synthesize_contract = WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: vec![
                "file_exists:$artifact".into(),
                "file_size_min:$artifact:1024".into(),
            ],
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Voice synthesis failed".into()],
            on_completion: vec![SpawnTaskValidatorSpec::Bare(
                ValidatorSpec::AudioNonSilent {
                    glob: "skill-output/voice/*.{mp3,wav}".into(),
                    min_ratio: default_non_silent_ratio(),
                },
            )],
        };

        // Voice save (custom voice registration): assert the voice was
        // actually registered with ominix-api. Closes the yangmi gap where
        // fm_voice_save returns success but the API has no record.
        let fm_voice_save_contract = WorkspaceSpawnTaskPolicy {
            artifact: None,
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Voice registration failed".into()],
            on_completion: vec![SpawnTaskValidatorSpec::Bare(
                ValidatorSpec::OminixVoiceExists {
                    name_arg: "name".into(),
                },
            )],
        };

        let mut spawn_tasks = BTreeMap::new();
        spawn_tasks.insert("fm_tts".into(), tts_contract.clone());
        spawn_tasks.insert("voice_synthesize".into(), voice_synthesize_contract);
        spawn_tasks.insert("podcast_generate".into(), podcast_contract);
        spawn_tasks.insert("fm_voice_save".into(), fm_voice_save_contract);

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
            compaction: None,
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

/// Variant of [`write_workspace_policy`] that fails closed when a
/// policy file is already present at `project_root`.
///
/// Implemented via a single `open(O_CREAT|O_EXCL)`-equivalent syscall
/// (`std::fs::OpenOptions::write(true).create_new(true)`) so two
/// concurrent callers — or a caller racing an operator hand-edit —
/// can never overwrite an existing `.octos-workspace.toml`.
/// `AlreadyExists` is treated as success, matching the "bootstrap
/// only if absent" idempotency contract M11-C relies on for the
/// per-session workspace policy.
///
/// This is intentionally a separate function from
/// [`write_workspace_policy`] so the legacy caller (which expects
/// truncate-on-write semantics for explicit policy edits) is
/// unchanged.
pub fn write_workspace_policy_if_absent(
    project_root: &Path,
    policy: &WorkspacePolicy,
) -> Result<()> {
    use std::io::Write;

    std::fs::create_dir_all(project_root)
        .wrap_err_with(|| format!("create project dir failed: {}", project_root.display()))?;
    let path = workspace_policy_path(project_root);
    let rendered = toml::to_string_pretty(policy)
        .wrap_err_with(|| format!("serialize workspace policy failed: {}", path.display()))?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => file
            .write_all(rendered.as_bytes())
            .wrap_err_with(|| format!("write workspace policy failed: {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "open workspace policy for create-new failed: {}",
                path.display()
            )
        }),
    }
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
            on_completion: Vec::new(),
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
            on_completion: Vec::new(),
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
            on_completion: Vec::new(),
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
            on_completion: Vec::new(),
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
            on_completion: Vec::new(),
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

    #[test]
    fn write_workspace_policy_if_absent_creates_file_when_missing() {
        let temp = tempfile::tempdir().unwrap();
        let policy = WorkspacePolicy::for_session();

        write_workspace_policy_if_absent(temp.path(), &policy).unwrap();

        let path = workspace_policy_path(temp.path());
        assert!(path.is_file());
        let roundtrip = read_workspace_policy(temp.path()).unwrap().unwrap();
        assert_eq!(roundtrip, policy);
    }

    #[test]
    fn write_workspace_policy_if_absent_preserves_existing_file() {
        // This is the M11-C contract: under concurrent bootstrap or
        // operator edit, a pre-existing `.octos-workspace.toml` is
        // never clobbered. Equivalent to `OpenOptions::create_new`
        // failing closed on `AlreadyExists`.
        let temp = tempfile::tempdir().unwrap();
        let path = workspace_policy_path(temp.path());
        let sentinel = "# operator hand-edit do not overwrite\n";
        std::fs::write(&path, sentinel).unwrap();

        // Should succeed (idempotent) but NOT overwrite.
        write_workspace_policy_if_absent(temp.path(), &WorkspacePolicy::for_session()).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, sentinel);
    }

    #[test]
    fn magic_byte_kind_matches_recognized_prefixes() {
        assert!(MagicByteKind::Mp3.matches(b"ID3\0\0"));
        assert!(MagicByteKind::Mp3.matches(&[0xFF, 0xFB, 0x90, 0x00]));
        assert!(!MagicByteKind::Mp3.matches(b"GIF87a"));

        assert!(MagicByteKind::Wav.matches(b"RIFFxxxxWAVE"));
        assert!(!MagicByteKind::Wav.matches(b"ID3xxxx"));

        assert!(MagicByteKind::Pdf.matches(b"%PDF-1.4"));
        assert!(MagicByteKind::Png.matches(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]));
        assert!(MagicByteKind::Jpeg.matches(&[0xFF, 0xD8, 0xFF, 0xE0]));

        // MP4: 'ftyp' must appear at byte offset 4 (after size prefix).
        let mp4: [u8; 16] = [
            0, 0, 0, 0x20, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm', 0, 0, 0, 0,
        ];
        assert!(MagicByteKind::Mp4.matches(&mp4));
    }

    #[test]
    fn session_policy_declares_new_domain_validators_for_silent_failure_paths() {
        let policy = WorkspacePolicy::for_session();
        let podcast = policy
            .spawn_tasks
            .get("podcast_generate")
            .expect("podcast contract");
        // The two new domain validators should be declared so the
        // "silent MP3" / "wrote HTML instead of MP3" failure modes are
        // caught at the contract gate.
        assert!(podcast.on_completion.iter().any(|entry| matches!(
            entry,
            SpawnTaskValidatorSpec::Bare(ValidatorSpec::AudioNonSilent { .. })
        )));
        assert!(podcast.on_completion.iter().any(|entry| matches!(
            entry,
            SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes { .. })
        )));

        let voice_save = policy
            .spawn_tasks
            .get("fm_voice_save")
            .expect("fm_voice_save contract");
        assert!(voice_save.on_completion.iter().any(|entry| matches!(
            entry,
            SpawnTaskValidatorSpec::Bare(ValidatorSpec::OminixVoiceExists { .. })
        )));

        let voice_synth = policy
            .spawn_tasks
            .get("voice_synthesize")
            .expect("voice_synthesize contract");
        assert!(voice_synth.on_completion.iter().any(|entry| matches!(
            entry,
            SpawnTaskValidatorSpec::Bare(ValidatorSpec::AudioNonSilent { .. })
        )));
    }

    #[test]
    fn spawn_task_validator_spec_roundtrips_through_toml_bare_and_full_forms() {
        // Bare form: just the spec table. id, required, phase auto-filled.
        let bare_toml = r#"
            kind = "ominix_voice_exists"
            name_arg = "name"
        "#;
        let bare: SpawnTaskValidatorSpec = toml::from_str(bare_toml).unwrap();
        let validator = bare.into_validator("fm_voice_save", 0);
        assert_eq!(validator.id, "fm_voice_save.on_completion[0]");
        assert!(validator.required);
        assert_eq!(validator.phase, ValidatorPhaseKind::Completion);
        match validator.spec {
            ValidatorSpec::OminixVoiceExists { ref name_arg } => {
                assert_eq!(name_arg, "name");
            }
            _ => panic!("expected OminixVoiceExists"),
        }

        // Full form: explicit id, required, phase.
        let full_toml = r#"
            id = "voice_optional"
            required = false
            phase = "completion"
            kind = "magic_bytes"
            glob = "*.mp3"
            format = "mp3"
        "#;
        let full: SpawnTaskValidatorSpec = toml::from_str(full_toml).unwrap();
        let validator = full.into_validator("ignored", 99);
        assert_eq!(validator.id, "voice_optional");
        assert!(!validator.required);
    }
}
